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
async fn file_tools_refuse_runtime_state_db() {
    let b = base();
    fs::write(b.join("state.sqlite"), b"not really a db").unwrap();
    let abs = format!("{}/state.sqlite", b.to_string_lossy().replace('\\', "/"));
    // 相对 / 绝对 / 伴生文件（即使此刻不存在）/ 大小写变体，读写删一律拒绝。
    for path in [
        "state.sqlite".to_string(),
        "STATE.SQLITE".to_string(),
        "state.sqlite-wal".to_string(),
        "agent/state.sqlite-shm".to_string(),
        abs,
    ] {
        let r = Read::new(b.clone()).execute(json!({"path": path})).await;
        assert!(r.is_error, "read {path} must be refused: {}", r.content);
        assert!(r.content.contains("运行时数据库"), "{}", r.content);
        let r = WriteFile::new(b.clone())
            .execute(json!({"path": path, "content": "x"}))
            .await;
        assert!(r.is_error, "write {path} must be refused");
        let r = DeleteFile::new(b.clone()).execute(json!({"path": path})).await;
        assert!(r.is_error, "delete {path} must be refused");
    }
    assert_eq!(fs::read(b.join("state.sqlite")).unwrap(), b"not really a db");
    // 别处的同名文件不受影响。
    let r = WriteFile::new(b.clone())
        .execute(json!({"path": "backups/state.sqlite", "content": "copy"}))
        .await;
    assert!(!r.is_error, "{}", r.content);
    // grep 从目录遍历进来也搜不到库文件的内容（别处的副本照常可搜）。
    let r = Grep::new(b.clone())
        .execute(json!({"pattern": "not really", "path": "."}))
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(r.content.contains("未找到匹配"), "{}", r.content);
    let r = Grep::new(b.clone())
        .execute(json!({"pattern": "copy", "path": "."}))
        .await;
    assert!(r.content.contains("backups/state.sqlite"), "{}", r.content);

    // 拼写别名在 Unrestricted 下也拦得住——那时沙箱的 `..` 过滤不再运行，只剩这道
    // 守卫：`.` / `..` 词法回退（Win32 打开前按词法消解，`agent/<不存在>/..` 仍是根）、
    // 保留名目录被 `..` 折掉（POSIX 的 create_dir_all 会先把它建出来）、指向库文件的
    // 符号链接；Windows 另有尾随点/空格归一、8.3 短名与 `\\?\` / `\\.\` 前缀 + 大小写变体。
    let mut aliases = vec![
        "./state.sqlite".to_string(),
        "agent/./state.sqlite-wal".to_string(),
        "agent/nonexistent/../state.sqlite".to_string(),
        "agent/nonexistent/../state.sqlite-journal".to_string(),
        "state.sqlite-journal/../x.txt".to_string(),
        "agent/state.sqlite-wal/./../y.txt".to_string(),
        // 非紧邻的 `..`：create_dir_all 逐段 mkdir 同样会先建出保留名目录。
        "state.sqlite-journal/sub/../../x.txt".to_string(),
        "state.sqlite-wal/a/b/../../../z.txt".to_string(),
    ];
    #[cfg(windows)]
    aliases.extend([
        "state.sqlite.".to_string(),
        "state.sqlite ".to_string(),
        "state.sqlite-journal.".to_string(),
        "state.sqlite:ads".to_string(),
        format!("\\\\?\\{}\\state.sqlite", b.to_string_lossy().to_uppercase()),
        format!("\\\\.\\{}\\state.sqlite-wal", b.to_string_lossy()),
    ]);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(b.join("state.sqlite"), b.join("link-to-db")).unwrap();
        aliases.push("link-to-db".to_string());
    }
    crate::access::scope_access(ToolAccess::Unrestricted, async {
        for path in aliases {
            let r = Read::new(b.clone()).execute(json!({"path": path})).await;
            assert!(r.is_error, "read {path} must be refused: {}", r.content);
            assert!(r.content.contains("运行时数据库"), "{path}: {}", r.content);
            let r = WriteFile::new(b.clone())
                .execute(json!({"path": path, "content": "x"}))
                .await;
            assert!(r.is_error, "write {path} must be refused: {}", r.content);
            let r = DeleteFile::new(b.clone()).execute(json!({"path": path})).await;
            assert!(r.is_error, "delete {path} must be refused: {}", r.content);
        }
    })
    .await;
    assert_eq!(fs::read(b.join("state.sqlite")).unwrap(), b"not really a db");
    for name in ["state.sqlite-journal", "state.sqlite-wal", "x.txt", "y.txt", "z.txt"] {
        assert!(!b.join(name).exists(), "不得借归一化创建假 hot journal / 副产物: {name}");
    }

    // 保留名作【目录】也拦（任何权限）：write 会 create_dir_all 出 agent/state.sqlite-journal/，
    // SQLite 下次打开把它当热 journal 读、读目录失败 → 整个库打不开。
    for path in [
        "state.sqlite-journal/x.txt",
        "agent/state.sqlite-wal/deep/x.txt",
        "State.SQLite-shm/x.txt",
    ] {
        let r = WriteFile::new(b.clone())
            .execute(json!({"path": path, "content": "x"}))
            .await;
        assert!(r.is_error, "write {path} must be refused: {}", r.content);
        assert!(r.content.contains("运行时数据库"), "{path}: {}", r.content);
        let r = Read::new(b.clone()).execute(json!({"path": path})).await;
        assert!(r.content.contains("运行时数据库"), "{path}: {}", r.content);
    }
    for name in ["state.sqlite-journal", "state.sqlite-wal", "State.SQLite-shm"] {
        assert!(!b.join(name).exists(), "不得以保留名建目录: {name}");
    }
    // 非保留名的目录照常可建。
    let r = WriteFile::new(b.clone())
        .execute(json!({"path": "state.sqlite-backup/x.txt", "content": "x"}))
        .await;
    assert!(!r.is_error, "{}", r.content);
}

/// 真正交给 `execute` 的「祖先」只有 tempdir 里多套的一层 `outer`：守卫一旦回归，最坏也只
/// 删掉本测试自己的临时目录，而不是 %TEMP% 或盘根（那两类只做纯函数断言，见下一个测试）。
#[tokio::test]
async fn delete_refuses_workspace_root_and_its_ancestors() {
    let dir = tempfile::tempdir().unwrap();
    let outer = dir.path().join("outer");
    let b = outer.join("agent");
    fs::create_dir_all(&b).unwrap();
    fs::write(b.join("state.sqlite"), b"not really a db").unwrap();
    fs::write(b.join("note.md"), b"keep me").unwrap();
    fs::write(outer.join("sibling.txt"), b"outer").unwrap();
    let root = b.to_string_lossy().replace('\\', "/");
    let parent = outer.to_string_lossy().replace('\\', "/");
    let root_spellings = [
        ".".to_string(),
        "./".to_string(),
        "agent".to_string(),
        "agent/.".to_string(),
        root.clone(),
        format!("{root}/"),
    ];
    let ancestor_spellings = ["..".to_string(), parent];
    // Unrestricted：根的各种拼法（相对 `.` / `agent` 前缀 / 绝对）与上级目录都由
    // contains_workspace_root 拒绝。
    crate::access::scope_access(ToolAccess::Unrestricted, async {
        for path in root_spellings.iter().chain(ancestor_spellings.iter()) {
            let r = DeleteFile::new(b.clone()).execute(json!({"path": path})).await;
            assert!(r.is_error, "delete {path} must be refused: {}", r.content);
            assert!(
                r.content.contains("工作区根目录") || r.content.contains("运行时数据库"),
                "{path}: {}",
                r.content
            );
        }
    })
    .await;
    // 沙箱：根的拼法走到同一道守卫；上级目录在沙箱门就被拒。
    for path in &root_spellings {
        let r = DeleteFile::new(b.clone()).execute(json!({"path": path})).await;
        assert!(r.is_error, "delete {path} must be refused: {}", r.content);
        assert!(
            r.content.contains("工作区根目录") || r.content.contains("运行时数据库"),
            "{path}: {}",
            r.content
        );
    }
    for path in &ancestor_spellings {
        let r = DeleteFile::new(b.clone()).execute(json!({"path": path})).await;
        assert!(r.is_error, "delete {path} must be refused: {}", r.content);
    }
    assert!(b.join("state.sqlite").exists());
    assert_eq!(fs::read(b.join("note.md")).unwrap(), b"keep me");
    assert_eq!(fs::read(outer.join("sibling.txt")).unwrap(), b"outer");
    // 根下的普通条目仍可删。
    let r = DeleteFile::new(b.clone())
        .execute(json!({"path": "note.md"}))
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(!b.join("note.md").exists());
}

/// 盘根 / 文件系统根绝不交给 `execute`（任何一道守卫回归都意味着 `remove_dir_all("C:\\")`）：
/// 只断言守卫函数本身。盘根从【规范化后的】base 祖先链取：%TEMP% 挂在 junction / subst
/// 盘上时词法祖先（`C:\`）与物理落点（`\\?\D:\…`）不在同一个盘，而守卫比的是物理落点。
#[test]
fn root_guards_recognise_drive_and_filesystem_roots() {
    let b = base();
    let canonical = b.canonicalize().unwrap();
    let drive_root = canonical.ancestors().last().unwrap().to_path_buf();
    assert!(drive_root.parent().is_none(), "{drive_root:?}");
    // 工作区所在盘的根是工作区祖先；同时也是文件系统根。
    assert!(contains_workspace_root(&b, &drive_root));
    assert!(is_filesystem_root(&drive_root));
    // 根上再 `..`：push_lexical 对 RootDir / Prefix 后的 `..` 不弹栈，仍停在根上。
    assert!(is_filesystem_root(&drive_root.join("..").join("..")));
    assert!(contains_workspace_root(&b, &drive_root.join("..").join("..")));
    // 与工作区无关的根：不是工作区祖先，但仍是根。只拿【不存在】的盘符试（存在的盘可能是
    // subst 盘，canonicalize 会解析到真实目录），它只走词法折叠分支；扩展长度前缀同理。
    #[cfg(windows)]
    {
        let missing = ('D'..='Z')
            .rev()
            .map(|letter| format!("{letter}:\\"))
            .find(|root| !Path::new(root).exists())
            .expect("总有一个不存在的盘符");
        let spellings = [
            missing.clone(),
            format!("{missing}..\\.."),
            format!("\\\\?\\{missing}"),
            format!("{}../..", missing.replace('\\', "/")),
        ];
        for spelling in &spellings {
            assert!(is_filesystem_root(Path::new(spelling)), "{spelling}");
            assert!(!contains_workspace_root(&b, Path::new(spelling)), "{spelling}");
        }
    }
    #[cfg(unix)]
    for spelling in ["/", "/../..", "/."] {
        assert!(is_filesystem_root(Path::new(spelling)), "{spelling}");
    }
    // 工作区自身、其子目录、其父目录都不是根。
    assert!(!is_filesystem_root(&b));
    assert!(!is_filesystem_root(&b.join("sub")));
    assert!(!is_filesystem_root(b.parent().unwrap()));
    // 叶子是指向根的链接：删的是链接条目本身，两道守卫都不按根算；带 `/.` 的拼写解析到
    // 根目录本身，仍按根拒绝。
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/", b.join("rootlink")).unwrap();
        assert!(!is_filesystem_root(&b.join("rootlink")));
        assert!(!contains_workspace_root(&b, &b.join("rootlink")));
        assert!(is_filesystem_root(&b.join("rootlink").join(".")));
    }
}

/// 根下叫保留名的【真实目录】（外部工具 / shell 建出来的假 hot journal）必须能从工具层
/// 删掉——它不是库文件，却会让 SQLite 开不了库；库文件本身、指向库文件的链接照旧拒绝，
/// 读 / 写 / 列目录对这个目录也依然拒绝（只开 delete 一个口子）。
#[tokio::test]
async fn delete_can_remove_stray_reserved_name_directory() {
    let b = base();
    fs::write(b.join("state.sqlite"), b"not really a db").unwrap();
    fs::create_dir_all(b.join("state.sqlite-journal").join("sub")).unwrap();
    fs::write(b.join("state.sqlite-journal").join("sub").join("x.txt"), b"x").unwrap();

    let r = Read::new(b.clone())
        .execute(json!({"path": "state.sqlite-journal/sub/x.txt"}))
        .await;
    assert!(r.is_error && r.content.contains("运行时数据库"), "{}", r.content);
    let r = ListDir::new(b.clone())
        .execute(json!({"path": "state.sqlite-journal"}))
        .await;
    assert!(r.is_error && r.content.contains("运行时数据库"), "{}", r.content);

    // 口子开给保留名目录下的【真实目录】（目录本身及其子目录：都不可能是库文件）；
    // 目录里的文件仍按库文件口径拒绝。
    let r = DeleteFile::new(b.clone())
        .execute(json!({"path": "state.sqlite-journal/sub/x.txt"}))
        .await;
    assert!(r.is_error && r.content.contains("运行时数据库"), "{}", r.content);
    assert!(b.join("state.sqlite-journal").join("sub").join("x.txt").exists());
    // 目录可删（递归），库文件本身仍不可删。
    let r = DeleteFile::new(b.clone())
        .execute(json!({"path": "state.sqlite-journal"}))
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(!b.join("state.sqlite-journal").exists());
    let r = DeleteFile::new(b.clone())
        .execute(json!({"path": "state.sqlite"}))
        .await;
    assert!(r.is_error, "{}", r.content);
    assert_eq!(fs::read(b.join("state.sqlite")).unwrap(), b"not really a db");

    #[cfg(unix)]
    {
        // 叫保留名、指向库文件的链接：物理落点就是库文件，照旧拒绝（不是真实目录，不开口子）。
        std::os::unix::fs::symlink(b.join("state.sqlite"), b.join("state.sqlite-wal")).unwrap();
        let r = DeleteFile::new(b.clone())
            .execute(json!({"path": "state.sqlite-wal"}))
            .await;
        assert!(r.is_error, "{}", r.content);
        assert!(fs::symlink_metadata(b.join("state.sqlite-wal")).is_ok());
        // 叫保留名、指向别处的链接（假 journal 残留）：write / edit / read 拒绝——原子写的
        // rename 会把链接换成一个真的叫保留名的文件；delete 放行，删的是链接条目本身。
        fs::write(b.join("note.md"), b"note").unwrap();
        std::os::unix::fs::symlink(b.join("note.md"), b.join("state.sqlite-journal")).unwrap();
        let r = WriteFile::new(b.clone())
            .execute(json!({"path": "state.sqlite-journal", "content": "fake journal"}))
            .await;
        assert!(r.is_error && r.content.contains("运行时数据库"), "{}", r.content);
        let r = EditFile::new(b.clone())
            .execute(json!({"path": "state.sqlite-journal", "old_string": "note", "new_string": "x"}))
            .await;
        assert!(r.is_error && r.content.contains("运行时数据库"), "{}", r.content);
        let r = Read::new(b.clone())
            .execute(json!({"path": "state.sqlite-journal"}))
            .await;
        assert!(r.is_error && r.content.contains("运行时数据库"), "{}", r.content);
        assert!(
            fs::symlink_metadata(b.join("state.sqlite-journal")).unwrap().file_type().is_symlink(),
            "链接必须原样保留，不能被换成真实文件"
        );
        assert_eq!(fs::read(b.join("note.md")).unwrap(), b"note");
        let r = DeleteFile::new(b.clone())
            .execute(json!({"path": "state.sqlite-journal"}))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(fs::symlink_metadata(b.join("state.sqlite-journal")).is_err());
        assert_eq!(fs::read(b.join("note.md")).unwrap(), b"note", "删的是链接，不是目标");
        // 指向工作区根的链接：删的是链接条目本身，不是工作区——必须放行，且根完好。
        std::os::unix::fs::symlink(&b, b.join("selflink")).unwrap();
        let r = DeleteFile::new(b.clone())
            .execute(json!({"path": "selflink"}))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(fs::symlink_metadata(b.join("selflink")).is_err());
        assert_eq!(fs::read(b.join("state.sqlite")).unwrap(), b"not really a db");
        // 带尾随分隔符 / `.` 的拼写解析到目录本身：仍按根拒绝。
        std::os::unix::fs::symlink(&b, b.join("selflink")).unwrap();
        for spelling in ["selflink/", "selflink/."] {
            let r = DeleteFile::new(b.clone())
                .execute(json!({"path": spelling}))
                .await;
            assert!(r.is_error, "{spelling}: {}", r.content);
        }
        assert!(fs::symlink_metadata(b.join("selflink")).is_ok());
        assert_eq!(fs::read(b.join("state.sqlite")).unwrap(), b"not really a db");
        // 叫 `state.sqlite` 本名的链接：可能就是库的入口（用户把库挪走后留下的链接），
        // delete 也拒绝；伴生名的链接（上面的 state.sqlite-journal）仍可清理。
        fs::rename(b.join("state.sqlite"), b.join("real.sqlite")).unwrap();
        std::os::unix::fs::symlink(b.join("real.sqlite"), b.join("state.sqlite")).unwrap();
        let r = DeleteFile::new(b.clone())
            .execute(json!({"path": "state.sqlite"}))
            .await;
        assert!(r.is_error && r.content.contains("运行时数据库"), "{}", r.content);
        assert!(fs::symlink_metadata(b.join("state.sqlite")).unwrap().file_type().is_symlink());
        fs::remove_file(b.join("state.sqlite")).unwrap();
        fs::write(b.join("note2.md"), b"n").unwrap();
        std::os::unix::fs::symlink(b.join("note2.md"), b.join("state.sqlite")).unwrap();
        let r = DeleteFile::new(b.clone())
            .execute(json!({"path": "state.sqlite"}))
            .await;
        assert!(r.is_error, "本名链接不看落点一律拒绝: {}", r.content);
        assert!(fs::symlink_metadata(b.join("state.sqlite")).is_ok());
    }
}

/// 工作区根本身是一个符号链接（`agent -> /data/real`）时：删 / 写这个条目等于把工作区入口
/// 拆掉（下次启动在原处新建一个空工作区）。「链接只删条目」的口子不适用于 base 自己的条目。
#[cfg(unix)]
#[tokio::test]
async fn symlinked_workspace_root_entry_is_neither_deletable_nor_writable() {
    let dir = tempfile::tempdir().unwrap();
    // 落点放在【另一个】临时目录：链接条目所在目录的保护（`contains_workspace_root` 第三条）
    // 只有在「落点不在该目录下」时才是决定性的——同一目录下按落点比就已经命中，分支测不到。
    let real_dir = tempfile::tempdir().unwrap();
    let real = real_dir.path().join("real");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("state.sqlite"), b"not really a db").unwrap();
    let b = dir.path().join("agent");
    std::os::unix::fs::symlink(&real, &b).unwrap();
    std::mem::forget(dir);
    let abs = b.to_string_lossy().into_owned();
    // `./agent` 不是根的拼法：`Path::starts_with` 按分量比，前导 `./` 让它不走「以根目录名
    // 开头的旧式相对路径」规则，落在工作区内的 `<base>/agent`（不存在）——与 rewind 的
    // `resolve_like_file_tool` 同口径，这里只确认它既不删根也不报根。
    let spellings = [
        "agent".to_string(),
        "agent/.".to_string(),
        abs.clone(),
        ".".to_string(),
        "./".to_string(),
    ];
    let r = DeleteFile::new(b.clone()).execute(json!({"path": "./agent"})).await;
    assert!(r.is_error && r.content.contains("文件不存在"), "{}", r.content);
    for access in [ToolAccess::Sandboxed, ToolAccess::Unrestricted] {
        crate::access::scope_access(access, async {
            for path in &spellings {
                let r = DeleteFile::new(b.clone()).execute(json!({"path": path})).await;
                assert!(r.is_error, "{access:?} delete {path}: {}", r.content);
                assert!(
                    r.content.contains("工作区根目录") || r.content.contains("运行时数据库"),
                    "{access:?} {path}: {}",
                    r.content
                );
                let r = WriteFile::new(b.clone())
                    .execute(json!({"path": path, "content": "x"}))
                    .await;
                assert!(r.is_error, "{access:?} write {path}: {}", r.content);
            }
        })
        .await;
    }
    assert!(fs::symlink_metadata(&b).unwrap().file_type().is_symlink(), "入口链接必须原样保留");
    assert_eq!(fs::read(real.join("state.sqlite")).unwrap(), b"not really a db");
    // 链接条目所在的目录及其祖先也不能删（递归删除会把入口条目一起带走）：按落点比
    // （real 不在 parent 下）看不见这一层，要按条目所在目录比。上一级只断言守卫。
    let parent = b.parent().unwrap().to_path_buf();
    assert!(
        !physical_normalize(&b).starts_with(physical_normalize(&parent)),
        "测试前提：落点不在链接所在目录下，否则第三条分支不是决定性的"
    );
    assert!(contains_workspace_root(&b, &parent));
    assert!(contains_workspace_root(&b, parent.parent().unwrap()));
    assert!(!contains_workspace_root(&b, &real.join("sub")), "工作区内的子目录不算");
    assert!(contains_workspace_root(&b, real_dir.path()), "落点的上级目录按落点比命中（第二条）");
    crate::access::scope_access(ToolAccess::Unrestricted, async {
        let r = DeleteFile::new(b.clone())
            .execute(json!({"path": parent.to_string_lossy()}))
            .await;
        assert!(r.is_error && r.content.contains("工作区根目录"), "{}", r.content);
    })
    .await;
    assert!(fs::symlink_metadata(&b).unwrap().file_type().is_symlink());
    // 同一父目录下、名字不同但落点与 base 相同的链接按根条目算（大小写不敏感文件系统 /
    // 8.3 短名的别名走的就是这条：名字对不上、落点相同）；落点不同的兄弟链接不算。
    std::os::unix::fs::symlink(&real, parent.join("alias")).unwrap();
    fs::create_dir_all(parent.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink(parent.join("elsewhere"), parent.join("other")).unwrap();
    assert!(is_workspace_root_entry(&b, &parent.join("alias")));
    assert!(contains_workspace_root(&b, &parent.join("alias")));
    assert!(!is_workspace_root_entry(&b, &parent.join("other")));
    assert!(!contains_workspace_root(&b, &parent.join("other")));
    // 工作区里的普通文件照常读写删。
    let r = WriteFile::new(b.clone())
        .execute(json!({"path": "note.md", "content": "hi"}))
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(fs::read(real.join("note.md")).unwrap(), b"hi");
    let r = DeleteFile::new(b.clone()).execute(json!({"path": "note.md"})).await;
    assert!(!r.is_error, "{}", r.content);
    // atomic_write 自己也拒绝覆盖指向目录的链接（工具守卫之外的第二道）。
    let err = rclaw_store::atomic_write(&b, b"x").unwrap_err().to_string();
    assert!(err.contains("指向目录的符号链接"), "{err}");
    assert!(fs::symlink_metadata(&b).unwrap().file_type().is_symlink());
}

/// Windows 盘符相对路径（`C:`、`C:foo`）：落点是该盘的进程级「当前目录」，不表示一个确定的
/// 文件；任何权限下都拒绝。类 Unix 下 `C:foo` 只是个普通文件名。
#[tokio::test]
async fn drive_relative_paths_are_refused_on_windows() {
    let b = base();
    #[cfg(windows)]
    {
        let drive = b
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .chars()
            .next()
            .unwrap();
        for access in [ToolAccess::Sandboxed, ToolAccess::Unrestricted] {
            crate::access::scope_access(access, async {
                for path in [format!("{drive}:"), format!("{drive}:foo.txt"), "Z:".to_string(), "Z:x".to_string()] {
                    let r = Read::new(b.clone()).execute(json!({"path": path})).await;
                    assert!(r.is_error && r.content.contains("盘符相对路径"), "{access:?} read {path}: {}", r.content);
                    let r = WriteFile::new(b.clone())
                        .execute(json!({"path": path, "content": "x"}))
                        .await;
                    assert!(r.is_error && r.content.contains("盘符相对路径"), "{access:?} write {path}: {}", r.content);
                    let r = DeleteFile::new(b.clone()).execute(json!({"path": path})).await;
                    assert!(r.is_error && r.content.contains("盘符相对路径"), "{access:?} delete {path}: {}", r.content);
                    let r = ListDir::new(b.clone()).execute(json!({"path": path})).await;
                    assert!(r.is_error && r.content.contains("盘符相对路径"), "{access:?} list {path}: {}", r.content);
                }
            })
            .await;
        }
        // 带根的盘符路径照常按沙箱 / 数据库规则处理（沙箱下工作区外被拒，但不是「盘符相对」）。
        let r = Read::new(b.clone()).execute(json!({"path": format!("{drive}:\\")})).await;
        assert!(r.is_error && !r.content.contains("盘符相对路径"), "{}", r.content);
    }
    #[cfg(not(windows))]
    {
        let r = WriteFile::new(b.clone())
            .execute(json!({"path": "C:foo.txt", "content": "x"}))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(fs::read(b.join("C:foo.txt")).unwrap(), b"x");
    }
}

/// `expected_sha256` 校验要先按 metadata 确认是普通文件再打开：FIFO 的 `open` 会一直等到有
/// 写端出现，工具调用就挂死了。
#[cfg(unix)]
#[tokio::test]
async fn write_version_check_does_not_open_special_files() {
    let b = base();
    let fifo = b.join("pipe");
    if !std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        WriteFile::new(b.clone()).execute(json!({
            "path": "pipe",
            "content": "x",
            "expected_sha256": "0".repeat(64)
        })),
    )
    .await
    .expect("版本校验不得阻塞在 FIFO 的 open 上");
    assert!(r.is_error && r.content.contains("不是普通文件"), "{}", r.content);
}

/// Linux：挂载点按根处理（`/proc` 与 `/` 不在同一设备）；`/proc/self` 是链接，叶子链接不算。
#[cfg(target_os = "linux")]
#[test]
fn mount_points_count_as_filesystem_roots() {
    if !Path::new("/proc/self").exists() {
        return;
    }
    assert!(is_filesystem_root(Path::new("/proc")));
    assert!(!is_filesystem_root(Path::new("/proc/self")));
    // 挂载点之下的普通目录不是根。
    assert!(!is_filesystem_root(Path::new("/proc/sys")));
}

/// Windows junction（`mklink /J`）：`is_symlink` 为真，删除只摘条目；指向工作区根的 junction
/// 可删且目标完好——Win32 在路径规范化阶段就把尾随 `\.` 折掉，所以 `selflink\.` 与 `selflink`
/// 是同一个条目（不像 POSIX 那样穿过链接），两种拼写都只摘 junction；指向盘根的 junction
/// 只断言守卫；叫保留名的 junction 可清理（不是库文件、不是本名）。
#[cfg(windows)]
#[tokio::test]
async fn windows_junctions_are_removed_as_entries_only() {
    let b = base();
    fs::write(b.join("state.sqlite"), b"not really a db").unwrap();
    let mklink = |link: &Path, target: &Path| {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|out| out.status.success())
    };
    if !mklink(&b.join("selflink"), &b) {
        return;
    }
    assert!(fs::symlink_metadata(b.join("selflink")).unwrap().file_type().is_symlink());
    let r = DeleteFile::new(b.clone()).execute(json!({"path": "selflink"})).await;
    assert!(!r.is_error, "{}", r.content);
    assert!(fs::symlink_metadata(b.join("selflink")).is_err());
    assert_eq!(fs::read(b.join("state.sqlite")).unwrap(), b"not really a db");
    // Win32 把 `selflink\.` / `selflink/.` 规范化成 `selflink` 本身（symlink_metadata 看到的
    // 就是 junction），所以删的仍只是条目，工作区内容完好。
    for spelling in ["selflink\\.", "selflink/."] {
        assert!(mklink(&b.join("selflink"), &b));
        let r = DeleteFile::new(b.clone()).execute(json!({"path": spelling})).await;
        assert!(!r.is_error, "{spelling}: {}", r.content);
        assert!(fs::symlink_metadata(b.join("selflink")).is_err(), "{spelling}");
        assert_eq!(fs::read(b.join("state.sqlite")).unwrap(), b"not really a db");
    }
    // 指向盘根的 junction：与 unix 的 rootlink 同口径只断言守卫（盘根绝不交给 execute）——
    // 叶子是链接时两道守卫都不按根算；`rootlink\.` 被 Win32 折成 junction 本身，同样是
    // 链接条目（POSIX 上才会穿过链接落到根）。
    let drive_root = b.canonicalize().unwrap().ancestors().last().unwrap().to_path_buf();
    assert!(mklink(&b.join("rootlink"), &drive_root));
    for spelling in [b.join("rootlink"), b.join("rootlink").join(".")] {
        assert!(!is_filesystem_root(&spelling), "{}", spelling.display());
        assert!(!contains_workspace_root(&b, &spelling), "{}", spelling.display());
    }
    fs::remove_dir(b.join("rootlink")).unwrap();
    assert!(drive_root.exists());
    // 叫伴生保留名的 junction：read / write 拒绝，delete 清理条目。
    fs::create_dir_all(b.join("dir")).unwrap();
    assert!(mklink(&b.join("state.sqlite-journal"), &b.join("dir")));
    let r = WriteFile::new(b.clone())
        .execute(json!({"path": "state.sqlite-journal", "content": "x"}))
        .await;
    assert!(r.is_error && r.content.contains("运行时数据库"), "{}", r.content);
    let r = DeleteFile::new(b.clone())
        .execute(json!({"path": "state.sqlite-journal"}))
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(fs::symlink_metadata(b.join("state.sqlite-journal")).is_err());
    assert!(b.join("dir").exists(), "删的是 junction 条目，不是目标目录");
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
