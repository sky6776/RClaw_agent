use super::*;
use rclaw_core::ToolAccess;
use serde_json::json;

fn base() -> PathBuf {
    let dir = tempfile::tempdir().unwrap();
    // 用一个静态目录做 base：返回其路径，tempdir 对象会自动清理。
    let p = dir.path().join("agent");
    fs::create_dir_all(&p).unwrap();
    // 泄漏 tempdir 使其在测试期间存活。
    std::mem::forget(dir);
    p
}

#[test]
fn local_path_schemas_describe_relative_resolution() {
    let b = base();
    for tool in [
        Read::new(b.clone()).input_schema(),
        WriteFile::new(b.clone()).input_schema(),
        EditFile::new(b.clone()).input_schema(),
        ListDir::new(b.clone()).input_schema(),
        Grep::new(b).input_schema(),
    ] {
        let description = tool["properties"]["path"]["description"]
            .as_str()
            .expect("path description");
        assert!(
            description.to_ascii_lowercase().contains("relative"),
            "path schema must describe relative-path resolution: {description}"
        );
    }
}

#[tokio::test]
async fn write_then_read_roundtrip() {
    let b = base();
    let wf = WriteFile::new(b.clone());
    let rf = Read::new(b.clone());
    assert_eq!(wf.name(), "write");

    let rel = format!("{}/memory/x.md", b.to_string_lossy().replace('\\', "/"));
    let r = wf.execute(json!({"path": rel, "content":"hello"})).await;
    assert!(!r.is_error, "{}", r.content);

    let r = rf.execute(json!({"path": rel})).await;
    assert!(!r.is_error);
    assert_eq!(r.content, "hello");
}

#[tokio::test]
async fn file_tools_resolve_relative_paths_from_agent_root() {
    let b = base();
    let wf = WriteFile::new(b.clone());
    let rf = Read::new(b.clone());

    let r = wf
        .execute(json!({
            "path": "skills/ai-berkshire/reports/中文 报告.md",
            "content": "relative-ok"
        }))
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(
        fs::read_to_string(
            b.join("skills")
                .join("ai-berkshire")
                .join("reports")
                .join("中文 报告.md")
        )
        .unwrap(),
        "relative-ok"
    );

    let r = rf
        .execute(json!({"path": "skills/ai-berkshire/reports/中文 报告.md"}))
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(r.content, "relative-ok");

    let explicitly_prefixed = b
        .file_name()
        .expect("temporary agent directory name")
        .to_string_lossy()
        .replace('\\', "/");
    let r = rf
        .execute(json!({
            "path": format!("{explicitly_prefixed}/skills/ai-berkshire/reports/中文 报告.md")
        }))
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(r.content, "relative-ok");
}

#[tokio::test]
async fn read_metadata_can_guard_write_and_edit() {
    let b = base();
    let wf = WriteFile::new(b.clone());
    let ef = EditFile::new(b.clone());
    let rf = Read::new(b.clone());
    let path = format!("{}/guarded.txt", b.to_string_lossy().replace('\\', "/"));
    fs::write(&path, "first").unwrap();

    let read = rf
        .execute(json!({"path": path, "include_metadata": true}))
        .await;
    let expected = sha256_bytes(b"first");
    assert!(read
        .content
        .starts_with(&format!("[read-metadata: sha256={expected} bytes=5]\n")));

    fs::write(&path, "changed elsewhere").unwrap();
    let stale_write = wf
        .execute(json!({
            "path": path,
            "content": "lost update",
            "expected_sha256": expected
        }))
        .await;
    assert!(stale_write.is_error);
    assert!(stale_write.content.contains("VERSION_CONFLICT"));
    assert_eq!(fs::read_to_string(&path).unwrap(), "changed elsewhere");

    let current = sha256_bytes(b"changed elsewhere");
    let edited = ef
        .execute(json!({
            "path": path,
            "old_string": "elsewhere",
            "new_string": "safely",
            "expected_sha256": current
        }))
        .await;
    assert!(!edited.is_error, "{}", edited.content);
    assert_eq!(fs::read_to_string(&path).unwrap(), "changed safely");
    assert!(edited.content.contains("sha256="));
}

#[tokio::test]
async fn write_missing_is_create_only() {
    let b = base();
    let wf = WriteFile::new(b.clone());
    let path = format!("{}/new.txt", b.to_string_lossy().replace('\\', "/"));

    let created = wf
        .execute(json!({
            "path": path,
            "content": "one",
            "expected_sha256": "missing"
        }))
        .await;
    assert!(!created.is_error, "{}", created.content);
    let conflict = wf
        .execute(json!({
            "path": path,
            "content": "two",
            "expected_sha256": "missing"
        }))
        .await;
    assert!(conflict.is_error);
    assert_eq!(fs::read_to_string(&path).unwrap(), "one");
}

#[tokio::test]
async fn concurrent_writes_with_same_version_allow_exactly_one() {
    let b = base();
    let wf = Arc::new(WriteFile::new(b.clone()));
    let path = format!("{}/race.txt", b.to_string_lossy().replace('\\', "/"));
    fs::write(&path, "base").unwrap();
    let expected = sha256_bytes(b"base");

    let left = {
        let wf = wf.clone();
        let path = path.clone();
        let expected = expected.clone();
        tokio::spawn(async move {
            wf.execute(json!({
                "path": path,
                "content": "left",
                "expected_sha256": expected
            }))
            .await
        })
    };
    let right = {
        let wf = wf.clone();
        let path = path.clone();
        tokio::spawn(async move {
            wf.execute(json!({
                "path": path,
                "content": "right",
                "expected_sha256": expected
            }))
            .await
        })
    };
    let (left, right) = tokio::join!(left, right);
    let results = [left.unwrap(), right.unwrap()];

    assert_eq!(results.iter().filter(|result| !result.is_error).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_error).count(), 1);
    assert!(matches!(
        fs::read_to_string(&path).unwrap().as_str(),
        "left" | "right"
    ));
}

#[cfg(windows)]
#[test]
fn windows_missing_path_case_aliases_share_mutation_lock() {
    let b = base();
    let upper = b.join("PendingCreate.txt").to_string_lossy().into_owned();
    let lower = b.join("pendingcreate.txt").to_string_lossy().into_owned();

    assert_eq!(normalize_lock_path(&upper), normalize_lock_path(&lower));
    assert!(Arc::ptr_eq(
        &file_mutation_lock(&upper),
        &file_mutation_lock(&lower)
    ));
}

#[tokio::test]
async fn rejects_path_traversal() {
    let b = base();
    let rf = Read::new(b.clone());
    let bad = format!("{}/../secret", b.to_string_lossy().replace('\\', "/"));
    let r = rf.execute(json!({"path": bad})).await;
    assert!(r.is_error);
}

#[tokio::test]
async fn read_attaches_local_media_file() {
    let b = base();
    let root = b.to_string_lossy().replace('\\', "/");
    let rf = Read::new(b.clone());
    let rel = format!("{root}/image.png");
    fs::write(&rel, b"png-bytes").unwrap();

    let r = rf.execute(json!({"path": rel})).await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(r.attachments.len(), 1);
    assert!(r.content.contains("image/png"));
    match &r.attachments[0] {
        ContentBlock::MediaUrl { kind, url } => {
            assert_eq!(*kind, MediaKind::Image);
            assert!(url.starts_with("data:image/png;base64,"));
        }
        other => panic!("unexpected attachment: {other:?}"),
    }
}

#[tokio::test]
async fn read_attaches_remote_media_url() {
    let rf = Read::new(base());
    let url = "https://example.com/demo.mp4?token=1";

    let r = rf.execute(json!({"url": url})).await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(r.attachments.len(), 1);
    match &r.attachments[0] {
        ContentBlock::MediaUrl { kind, url: got } => {
            assert_eq!(*kind, MediaKind::Video);
            assert_eq!(got, url);
        }
        other => panic!("unexpected attachment: {other:?}"),
    }
}

/// 空 old_string 必须报错，而不是在文件开头插入内容。
#[tokio::test]
async fn edit_rejects_empty_old_string() {
    let b = base();
    let wf = WriteFile::new(b.clone());
    let ef = EditFile::new(b.clone());
    let rel = format!("{}/e.txt", b.to_string_lossy().replace('\\', "/"));
    wf.execute(json!({"path": rel, "content":"hello"})).await;
    let r = ef
        .execute(json!({"path": rel, "old_string":"", "new_string":"x"}))
        .await;
    assert!(r.is_error, "空 old_string 应被拒绝");
}

/// P2：edit 不得把超大文件整篇读进内存后才拒绝；应像 read 一样用 metadata
/// 提前拦截（复用 read_text_bounded）。写一个 > MAX_FILE_SIZE 的文件，确认 edit
/// 返回错误而不发生大分配。
#[tokio::test]
async fn edit_rejects_oversized_file() {
    let b = base();
    let ef = EditFile::new(b.clone());
    let rel = format!("{}/big.txt", b.to_string_lossy().replace('\\', "/"));
    // 用 set_len 制造【稀疏文件】：meta.len() 报告 > MAX_FILE_SIZE，但不实际写满
    // 磁盘/内存。确认 edit 在 metadata 阶段即拒绝，不会把超大文件整篇读进内存。
    let f = std::fs::File::create(&rel).unwrap();
    f.set_len((MAX_FILE_SIZE + 1024) as u64).unwrap();
    let r = ef
        .execute(json!({"path": rel, "old_string":"a", "new_string":"b"}))
        .await;
    assert!(r.is_error, "超大文件应被限流读取拦截");
}

#[tokio::test]
async fn list_returns_written_file() {
    let b = base();
    let wf = WriteFile::new(b.clone());
    let ld = ListDir::new(b.clone());
    let rel = format!("{}/a.txt", b.to_string_lossy().replace('\\', "/"));
    wf.execute(json!({"path": rel, "content":"x"})).await;
    let r = ld.execute(json!({})).await;
    assert!(r.content.contains("a.txt"));
}

#[tokio::test]
async fn delete_removes_file() {
    let b = base();
    let wf = WriteFile::new(b.clone());
    let df = DeleteFile::new(b.clone());
    let rel = format!("{}/to_remove.txt", b.to_string_lossy().replace('\\', "/"));
    wf.execute(json!({"path": rel, "content":"x"})).await;
    assert!(b.join("to_remove.txt").exists());
    let r = df.execute(json!({"path": rel})).await;
    assert!(!r.is_error, "{:?}", r.content);
    assert!(!b.join("to_remove.txt").exists());
}

#[tokio::test]
async fn delete_removes_directory_recursively() {
    let b = base();
    let df = DeleteFile::new(b.clone());
    let dir = b.join("to_remove_dir");
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::fs::write(dir.join("nested").join("child.txt"), "y").unwrap();
    let rel = format!("{}/to_remove_dir", b.to_string_lossy().replace('\\', "/"));
    let r = df.execute(json!({"path": rel})).await;
    assert!(!r.is_error, "{:?}", r.content);
    assert!(!dir.exists());
}

#[tokio::test]
async fn delete_missing_file_is_error() {
    let b = base();
    let df = DeleteFile::new(b.clone());
    let rel = format!("{}/nope.txt", b.to_string_lossy().replace('\\', "/"));
    let r = df.execute(json!({"path": rel})).await;
    assert!(r.is_error, "删除不存在的文件应为错误");
}

/// delete 的权限与 write 一致：Sandboxed（默认）拒绝 agent 外路径；
/// Unrestricted（max QQ）放行任意路径。
#[tokio::test]
async fn delete_permission_matches_write() {
    let b = base();
    let df = DeleteFile::new(b.clone());
    let outside = std::env::temp_dir().join("rclaw_delete_perm_probe.txt");
    std::fs::write(&outside, "z").unwrap();
    let outside_str = outside.to_string_lossy().to_string();

    // Sandboxed（默认，无 task-local）：agent 外路径被拒，且不删文件。
    let denied = df.execute(json!({"path": outside_str.clone()})).await;
    assert!(denied.is_error, "Sandboxed 应拒绝 agent 外路径");
    assert!(outside.exists(), "被拒时不应删除文件");

    // Unrestricted（max QQ）：放行 agent 外路径并删除。
    crate::access::scope_access(ToolAccess::Unrestricted, async {
        let ok = df.execute(json!({"path": outside_str})).await;
        assert!(!ok.is_error, "Unrestricted 应允许删除: {:?}", ok.content);
    })
    .await;
    assert!(!outside.exists(), "Unrestricted 删除后文件应不存在");
    let _ = std::fs::remove_file(&outside);
}

/// list 的 path 参数：Sandboxed 拒绝 agent 外路径；Unrestricted（max）可列出。
#[tokio::test]
async fn list_path_respects_access() {
    use rclaw_core::ToolAccess;
    let b = base();
    // agent 外目录 + 文件。
    let outside_dir = tempfile::tempdir().unwrap();
    let outside = outside_dir.path();
    fs::write(outside.join("outside_file.txt"), "x").unwrap();
    let outside_str = outside.to_string_lossy().replace('\\', "/");

    let ld = ListDir::new(b.clone());

    // Sandboxed（默认）：agent 外路径 → resolve_path 拒绝。
    let r = ld.execute(json!({"path": outside_str.clone()})).await;
    assert!(r.is_error, "sandboxed list 必须拒绝 agent 外路径");

    // Unrestricted（max）：agent 外路径 → 列出。
    let r2 = crate::access::scope_access(ToolAccess::Unrestricted, async move {
        ld.execute(json!({"path": outside_str})).await
    })
    .await;
    assert!(!r2.is_error, "unrestricted list 应能列出 agent 外路径");
    assert!(r2.content.contains("outside_file.txt"));
}

/// 目录符号链接绝不能被跟随：在 base 内放一个指向 base 之外的符号链接目录，
/// list 只能列出该符号链接自身，不得列出（更不得递归）其目标内容。
/// Windows 下创建符号链接需特权/开发者模式，故仅在 unix 测试。
#[cfg(unix)]
#[tokio::test]
async fn list_does_not_follow_symlink_dir() {
    let b = base();
    // base 之外建一个「诱饵」目录并写一个特征文件。
    let outside = std::env::temp_dir().join("rclaw_symlink_lure");
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("SECRET_LEAK.txt"), "must-not-appear").unwrap();

    // 在 base 内放符号链接指向诱饵目录。
    let link_path = b.join("evil_link");
    let _ = std::os::unix::fs::symlink(&outside, &link_path);

    let ld = ListDir::new(b.clone());
    let r = ld.execute(json!({})).await;
    // 符号链接自身的路径会被列出（它位于 base 下）……
    assert!(r.content.contains("evil_link"));
    // ……但绝不能把目标目录里的文件列出来。
    assert!(
        !r.content.contains("SECRET_LEAK.txt"),
        "符号链接目录不得被跟随：{contents}",
        contents = r.content
    );

    let _ = fs::remove_dir_all(&outside);
}

/// 分页读取：大文件不再被 32KB 硬拒，默认返回首窗口 + 翻页提示；offset 可续读。
#[tokio::test]
async fn read_paginates_large_file() {
    let b = base();
    let root = b.to_string_lossy().replace('\\', "/");
    let wf = WriteFile::new(b.clone());
    let rf = Read::new(b.clone());
    let rel = format!("{root}/big.txt");
    // 造一个远超 32KB 的文件（旧实现会直接「文件过大」拒绝）。
    let mut content = String::new();
    for i in 0..10_000 {
        content.push_str(&format!("line{i}\n"));
    }
    wf.execute(json!({"path": rel, "content": content})).await;

    // 显式 limit=200 读取：返回前 200 行 + 「未完，offset=200」提示。
    // 不依赖默认 limit 常量——默认值后续调优不会让本测试失效。
    let r = rf.execute(json!({"path": rel, "limit": 200})).await;
    assert!(!r.is_error, "{}", r.content);
    assert!(r.content.contains("line0\n"));
    assert!(r.content.contains("line199\n"));
    assert!(!r.content.contains("line200\n"));
    assert!(r.content.contains("offset=200"));

    // 翻页：offset=200 取下一段。
    let r = rf
        .execute(json!({"path": rel, "offset": 200, "limit": 5}))
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(r.content.contains("line200\n"));
    assert!(r.content.contains("line204\n"));
    assert!(r.content.contains("offset=205"));
}

/// 小文件整篇读取仍原样返回（无翻页提示），与旧行为一致。
#[tokio::test]
async fn read_small_file_no_note() {
    let b = base();
    let root = b.to_string_lossy().replace('\\', "/");
    let wf = WriteFile::new(b.clone());
    let rf = Read::new(b.clone());
    let rel = format!("{root}/small.txt");
    wf.execute(json!({"path": rel, "content":"only line"}))
        .await;
    let r = rf.execute(json!({"path": rel})).await;
    assert!(!r.is_error);
    assert_eq!(r.content, "only line");
}

/// limit=0 不能被当作「返回 0 行」再误报 offset 超范围；应直接给出参数错误。
#[tokio::test]
async fn read_rejects_zero_limit() {
    let b = base();
    let root = b.to_string_lossy().replace('\\', "/");
    let wf = WriteFile::new(b.clone());
    let rf = Read::new(b.clone());
    let rel = format!("{root}/small.txt");
    wf.execute(json!({"path": rel, "content":"only line"}))
        .await;
    let r = rf.execute(json!({"path": rel, "limit": 0})).await;
    assert!(r.is_error);
    assert!(r.content.contains("limit 必须大于 0"), "{}", r.content);
}

/// 显式非法 offset 不能静默退回默认 0，否则调用方会误以为读到了请求窗口。
#[tokio::test]
async fn read_rejects_negative_offset() {
    let b = base();
    let root = b.to_string_lossy().replace('\\', "/");
    let wf = WriteFile::new(b.clone());
    let rf = Read::new(b.clone());
    let rel = format!("{root}/small.txt");
    wf.execute(json!({"path": rel, "content":"only line"}))
        .await;
    let r = rf.execute(json!({"path": rel, "offset": -1})).await;
    assert!(r.is_error);
    assert!(r.content.contains("offset 必须是非负整数"), "{}", r.content);
}

/// grep：目录递归正则搜索，输出 file:line:content，未命中行不出现。
#[tokio::test]
async fn grep_searches_directory() {
    let b = base();
    let root = b.to_string_lossy().replace('\\', "/");
    let wf = WriteFile::new(b.clone());
    let gp = Grep::new(b.clone());
    wf.execute(json!({"path": format!("{root}/a.txt"), "content":"foo\nbar\nfoo baz\n"}))
        .await;
    wf.execute(json!({"path": format!("{root}/sub/b.txt"), "content":"bar\n"}))
        .await;

    let r = gp.execute(json!({"pattern":"foo", "path": root})).await;
    assert!(!r.is_error, "{}", r.content);
    assert!(r.content.contains(":1: foo"));
    assert!(r.content.contains(":3: foo baz"));
    assert!(!r.content.contains(":2: bar"));
}

/// grep：路径穿越与非法正则都必须报错。
#[tokio::test]
async fn grep_rejects_traversal_and_bad_regex() {
    let b = base();
    let root = b.to_string_lossy().replace('\\', "/");
    let gp = Grep::new(b.clone());
    let bad = format!("{root}/../secret");
    let r = gp.execute(json!({"pattern":"x", "path": bad})).await;
    assert!(r.is_error);

    let inside = format!("{root}/a.txt");
    let r = gp.execute(json!({"pattern":"(", "path": inside})).await;
    assert!(r.is_error, "非法正则应报错");
}

/// max_results=0 若被接受，会导致 grep 不扫描任何内容却返回“未找到匹配”。
#[tokio::test]
async fn grep_rejects_zero_max_results() {
    let b = base();
    let root = b.to_string_lossy().replace('\\', "/");
    let wf = WriteFile::new(b.clone());
    let gp = Grep::new(b.clone());
    let rel = format!("{root}/a.txt");
    wf.execute(json!({"path": rel, "content":"foo\n"})).await;
    let r = gp
        .execute(json!({"pattern":"foo", "path": rel, "max_results": 0}))
        .await;
    assert!(r.is_error);
    assert!(
        r.content.contains("max_results 必须大于 0"),
        "{}",
        r.content
    );
}

/// 即使没有匹配，只要大文件扫描被截断，也必须把“仅扫描了前缀”告诉调用方。
#[test]
fn grep_finish_mentions_capped_files_without_matches() {
    let acc = GrepAccum {
        out: String::new(),
        matches: 0,
        bytes_out: 0,
        capped_files: 1,
        full: false,
    };
    let r = finish_grep(acc, "needle");
    assert!(!r.is_error);
    assert!(r.content.contains("未找到匹配"), "{}", r.content);
    assert!(
        r.content.contains("1 个大文件仅扫描了前缀"),
        "{}",
        r.content
    );
}

/// grep：含 NUL 的二进制文件应被整体跳过（无命中），同 ripgrep 默认行为。
#[tokio::test]
async fn grep_skips_binary_file() {
    let b = base();
    let root = b.to_string_lossy().replace('\\', "/");
    let gp = Grep::new(b.clone());
    let rel = format!("{root}/bin.txt");
    // write 只接受 UTF-8 content，无法写 NUL；直接走原子写落盘二进制内容。
    rclaw_store::atomic_write(rel.as_str(), b"foo\x00bar\nbaz\n").unwrap();
    let r = gp.execute(json!({"pattern":"baz", "path": rel})).await;
    assert!(!r.is_error);
    assert!(r.content.contains("未找到匹配"), "{}", r.content);
}

/// grep：文件中夹杂非 UTF-8 行时，合法行仍应被匹配（非 UTF-8 行跳过、不中断扫描，
/// 行号仍连续）。回归 F1：确保 `continue` 路径不会让匹配/行号错乱。
#[tokio::test]
async fn grep_skips_non_utf8_lines() {
    let b = base();
    let root = b.to_string_lossy().replace('\\', "/");
    let gp = Grep::new(b.clone());
    let rel = format!("{root}/mix.txt");
    // 第 2 行为非法 UTF-8（0xff/0xfe），应被跳过；第 1、3 行匹配 "keep"，行号连续。
    rclaw_store::atomic_write(rel.as_str(), b"keep1\n\xff\xfe\nkeep2\n").unwrap();
    let r = gp.execute(json!({"pattern":"keep", "path": rel})).await;
    assert!(!r.is_error, "{}", r.content);
    assert!(r.content.contains(":1: keep1"), "{}", r.content);
    assert!(r.content.contains(":3: keep2"), "{}", r.content);
}

/// APK 审批摘除钩子（阶段 2.2）：write/edit/delete 触及 skills/**/*.py 时必须
/// 通知宿主钩子；非 skills 路径与无钩子时为零行为（D7）。
#[tokio::test]
async fn write_edit_delete_notify_skill_script_change_hook() {
    let seen: std::sync::Arc<std::sync::Mutex<Vec<PathBuf>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    crate::set_skill_script_change_hook({
        let seen = seen.clone();
        std::sync::Arc::new(move |path: &Path| {
            seen.lock().unwrap().push(path.to_path_buf());
        })
    });
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::clear_skill_script_change_hook();
        }
    }
    let _guard = HookGuard;

    let b = base();
    let wf = WriteFile::new(b.clone());
    let ef = EditFile::new(b.clone());
    let df = DeleteFile::new(b.clone());
    let root = b.to_string_lossy().replace('\\', "/");

    // 非 skills 路径：不通知。
    let r = wf
        .execute(json!({"path": format!("{root}/plain.txt"), "content": "x"}))
        .await;
    assert!(!r.is_error);

    // skills 下的 .py：写/编辑/删除都通知。
    let script = format!("{root}/skills/demo/scripts/run.py");
    let r = wf
        .execute(json!({"path": script, "content": "print('v1')\n"}))
        .await;
    assert!(!r.is_error, "{}", r.content);
    let r = ef
        .execute(json!({"path": script, "old_string": "v1", "new_string": "v2"}))
        .await;
    assert!(!r.is_error, "{}", r.content);
    let r = df.execute(json!({"path": script})).await;
    assert!(!r.is_error, "{}", r.content);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "写/编辑/删除各通知一次: {seen:?}");
    assert!(seen.iter().all(|p| p.to_string_lossy().contains("skills")));
}
