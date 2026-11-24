//! 手动验证：对象存储（OSS / S3 兼容）是否执行 `If-None-Match: *` 条件创建。
//!
//! 这是 RClaw 关闭协议里「空间标记只许第一个创建者写」的依据：服务端若忽略条件头，
//! 迟到的创建可能把已关闭的 `space.json` 改回开放（新版靠墓碑兜底，只认旧标记的客户端
//! 有边界）。部署新后端（尤其是阿里云 OSS 的 S3 兼容端点）前跑一次，确认第二次写入
//! 返回 412 且内容不变。
//!
//! 默认忽略，不碰网络；需要环境变量（或先 `source target/tmp/oss.env`）：
//!
//! ```text
//! RCLAW_OSS_ENDPOINT=https://s3.oss-cn-hangzhou.aliyuncs.com
//! RCLAW_OSS_REGION=cn-hangzhou          # 区域 ID，不是端点里的 oss-cn- 前缀
//! RCLAW_OSS_BUCKET=<桶名>
//! RCLAW_OSS_ACCESS_KEY_ID=<RAM 子账号 AK>
//! RCLAW_OSS_SECRET_KEY=<RAM 子账号 SK>
//! # 可选：
//! RCLAW_OSS_PREFIX=rclaw-cond-test-<随机>   # 默认 rclaw-cond-test-<pid>，测试对象都在它下面
//! RCLAW_OSS_PATH_STYLE=0                    # 1 = 路径式寻址（自建 MinIO 常用）
//! RCLAW_OSS_FORBID_OVERWRITE=0              # 1 = 阿里云 OSS：改用 x-oss-forbid-overwrite
//! ```
//!
//! 只在该前缀下写一个几十字节的测试对象，结束时删除；不要用生产前缀跑。
//!
//! ```bash
//! cargo test -p rclaw-sync --test oss_conditional -- --ignored --nocapture
//! ```

use rclaw_sync::transport::s3::{S3Options, S3Transport};
use rclaw_sync::transport::SyncTransport;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.trim().is_empty())
}

#[tokio::test]
#[ignore = "手动网络验证：需要 OSS/S3 凭据，见文件头"]
async fn oss_conditional_create_double_write() {
    let endpoint = env("RCLAW_OSS_ENDPOINT").expect("缺少 RCLAW_OSS_ENDPOINT");
    let region = env("RCLAW_OSS_REGION").expect("缺少 RCLAW_OSS_REGION");
    let bucket = env("RCLAW_OSS_BUCKET").expect("缺少 RCLAW_OSS_BUCKET");
    let access_key = env("RCLAW_OSS_ACCESS_KEY_ID").expect("缺少 RCLAW_OSS_ACCESS_KEY_ID");
    let secret_key = env("RCLAW_OSS_SECRET_KEY").expect("缺少 RCLAW_OSS_SECRET_KEY");
    let prefix = env("RCLAW_OSS_PREFIX")
        .unwrap_or_else(|| format!("rclaw-cond-test-{}", std::process::id()));
    let path_style = env("RCLAW_OSS_PATH_STYLE").as_deref() == Some("1");
    let oss_forbid_overwrite = env("RCLAW_OSS_FORBID_OVERWRITE").as_deref() == Some("1");
    // 本地 mock / 局域网 MinIO 用 http 时需要显式允许；公网地址仍然强制 HTTPS。
    let allow_http = env("RCLAW_OSS_ALLOW_HTTP").as_deref() == Some("1");

    let transport = S3Transport::new(&S3Options {
        endpoint: endpoint.clone(),
        region: region.clone(),
        bucket: bucket.clone(),
        prefix: prefix.clone(),
        access_key,
        secret_key,
        path_style,
        oss_forbid_overwrite,
        allow_http,
    })
    .unwrap_or_else(|error| panic!("构造 S3 传输失败（检查端点/区域/桶名）: {error}"));

    let key = "conditional-probe.txt";
    // 清理上一次的残留：删不掉也不影响后续（下面第一次条件写会拿到 412，能看出来）。
    let _ = transport.delete(key).await;

    println!(
        "endpoint={endpoint} region={region} bucket={bucket} prefix={prefix} path_style={path_style} oss_forbid_overwrite={oss_forbid_overwrite}"
    );

    transport
        .put_new(key, b"first".to_vec())
        .await
        .unwrap_or_else(|error| panic!("第一次条件创建失败（连不上/权限/签名？）: {error}"));
    println!("第一次 put_new: OK");

    match transport.put_new(key, b"second".to_vec()).await {
        Ok(()) => {
            let kept = transport.get(key, 64 * 1024).await.unwrap_or_default();
            panic!(
                "服务端没有执行「仅创建」（第二次 put_new 返回成功，内容={:?}）——不满足条件创建",
                String::from_utf8_lossy(&kept)
            );
        }
        Err(error) => {
            // 只认「已存在」这一种失败，而且按结构精确判定：错误串形如
            // `创建 <key>失败（HTTP 409 Conflict）（FileAlreadyExists）`。子串匹配会把
            // 403 里恰好带了 "412" 之类的内容判成通过。
            let rclaw_sync::TransportError::Other(message) = &error else {
                panic!("第二次写入不是预期的冲突类型: {error}");
            };
            let prefix = format!("创建 {key}失败（HTTP ");
            let rest = message
                .strip_prefix(prefix.as_str())
                .unwrap_or_else(|| panic!("第二次写入缺少 HTTP 状态: {error}"));
            let (status, tail) = rest
                .split_once('）')
                .unwrap_or_else(|| panic!("第二次写入的状态串不完整: {error}"));
            let expected = if oss_forbid_overwrite { "409" } else { "412" };
            assert_eq!(
                status.split_ascii_whitespace().next(),
                Some(expected),
                "第二次写入的状态码不对: {error}"
            );
            if oss_forbid_overwrite {
                assert_eq!(tail, "（FileAlreadyExists）", "OSS 错误码不对: {error}");
            }
            println!("第二次 put_new 被拒（状态码与错误码都正确）: {error}");
        }
    }

    let kept = transport
        .get(key, 64 * 1024)
        .await
        .unwrap_or_else(|error| panic!("读回失败: {error}"));
    assert_eq!(
        kept, b"first",
        "第二次写入被拒了，但读回内容不是第一次的：{:?}",
        String::from_utf8_lossy(&kept)
    );
    println!("读回内容仍是第一次写入的（{} 字节）", kept.len());

    transport
        .delete(key)
        .await
        .unwrap_or_else(|error| panic!("删除测试对象失败（请手动清理 {prefix}/{key}）: {error}"));
    println!("已清理测试对象；条件创建验收通过");
}
