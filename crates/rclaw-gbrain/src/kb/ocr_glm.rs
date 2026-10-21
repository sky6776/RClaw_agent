//! 智谱 GLM-OCR Provider — 通过 HTTP API 调用 GLM-OCR layout_parsing

use crate::error::{GBrainError, Result};
use crate::kb::ocr_planner::generate_request_id;
use crate::kb::ocr_provider::{OcrInput, OcrOptions, OcrPageResult, OcrProvider};
use crate::kb::ocr_response::{normalize_glm_ocr_response, GlmOcrResponse};

/// 智谱 GLM-OCR provider 实现
#[derive(Clone)]
pub struct GlmOcrProvider {
    /// API key
    api_key: String,
}

const PDF_DATA_URI_PREFIX: &str = "data:application/pdf;base64,";
const PNG_DATA_URI_PREFIX: &str = "data:image/png;base64,";
const JPEG_DATA_URI_PREFIX: &str = "data:image/jpeg;base64,";

impl GlmOcrProvider {
    pub fn new(api_key: &str) -> Self {
        Self {
            api_key: api_key.to_string(),
        }
    }

    /// 在独立普通线程上执行 [`OcrProvider::recognize`]。
    ///
    /// reqwest::blocking 的 client 禁止在 tokio async 上下文（`rt.block_on` 内）
    /// 构造：其内部 runtime 在 async 上下文中 drop 时直接 panic
    /// （"Cannot drop a runtime in a context where blocking is not allowed"），
    /// 且 panic 走 stderr 不进 tracing 日志。KB pipeline 的
    /// `process_document_async` 在 `rt.block_on` 内同步调用本 provider，
    /// DOCX 内嵌图片 OCR 曾因此在 kb-worker 线程上静默崩溃，job 永久卡 running。
    /// 所有调用方必须经由此方法而非直接调 `recognize`。
    pub fn recognize_off_async_context(
        &self,
        input: &OcrInput,
        options: &OcrOptions,
    ) -> Result<Vec<OcrPageResult>> {
        let provider = self.clone();
        let input = input.clone();
        let options = options.clone();
        std::thread::Builder::new()
            .name("glm-ocr-call".to_string())
            .spawn(move || provider.recognize(&input, &options))
            .map_err(|e| GBrainError::Http(format!("GLM-OCR 调用线程创建失败: {}", e)))?
            .join()
            .map_err(|payload| {
                let detail = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown panic".to_string());
                GBrainError::Http(format!("GLM-OCR 调用线程 panic: {}", detail))
            })?
    }
}

impl OcrProvider for GlmOcrProvider {
    fn name(&self) -> &'static str {
        "glm_ocr"
    }

    fn recognize(&self, input: &OcrInput, options: &OcrOptions) -> Result<Vec<OcrPageResult>> {
        if let OcrInput::Image {
            file,
            mime_type,
            document_id,
            run_id,
        } = input
        {
            return self.recognize_image(file, mime_type, *document_id, run_id, options);
        }

        let OcrInput::PdfRange {
            file,
            request_start_page_id,
            request_end_page_id,
            source_start_page,
            source_end_page,
            document_id,
            run_id,
        } = input
        else {
            unreachable!("image OCR inputs are handled before PDF processing")
        };

        let timeout = std::time::Duration::from_secs(
            options.timeout_seconds_per_page
                * (*request_end_page_id - *request_start_page_id + 1) as u64,
        );

        // MaaS layout_parsing expects local PDF bytes as a data URI, not bare base64.
        let file_content = match file {
            crate::kb::ocr_provider::OcrFilePayload::Base64(data) => pdf_base64_to_data_uri(data),
            crate::kb::ocr_provider::OcrFilePayload::Url(_url) => {
                // 如果是 URL，先下载内容再 base64 编码
                // 第一版直接使用 base64 模式
                return Err(GBrainError::InvalidInput(
                    "GLM-OCR 当前仅支持 base64 模式".to_string(),
                ));
            }
        };

        let request_id =
            generate_request_id(*document_id, run_id, *source_start_page, *source_end_page);

        let mut body = serde_json::json!({
            "model": options.model,
            "file": file_content,
            "start_page_id": request_start_page_id,
            "end_page_id": request_end_page_id,
            "enable_layout": options.enable_layout,
            "request_id": request_id,
        });

        if options.return_crop_images {
            body["return_crop_images"] = serde_json::json!(true);
        }
        if options.need_layout_visualization {
            body["need_layout_visualization"] = serde_json::json!(true);
        }

        // 使用 blocking client 避免 tokio runtime 嵌套 panic
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| GBrainError::Http(format!("创建 HTTP client 失败: {}", e)))?;

        let response = client
            .post(&options.base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .map_err(|e| GBrainError::Http(format!("GLM-OCR 请求失败: {}", e)))?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().unwrap_or_default();
            return Err(GBrainError::Http(format!(
                "GLM-OCR API 错误 (status={}): {}",
                status, error_text
            )));
        }

        let response_json: serde_json::Value = response
            .json()
            .map_err(|e| GBrainError::Serialization(format!("GLM-OCR 响应解析失败: {}", e)))?;

        // 解析为 GlmOcrResponse
        let mut glm_response: GlmOcrResponse = serde_json::from_value(response_json.clone())
            .map_err(|e| GBrainError::Serialization(format!("GLM-OCR 响应结构解析失败: {}", e)))?;

        // 保留原始 JSON
        glm_response.raw_json = response_json;

        let request_page_count = (*source_end_page - *source_start_page + 1) as usize;

        // 多页请求只有 md_results 且无 layout_details 时，自动拆成单页请求重试。
        // 单页 md_results-only 是正常响应，无需重试。
        if request_page_count > 1 && glm_response.layout_details.is_empty() {
            if let Some(ref md) = glm_response.md_results {
                if !md.is_empty() {
                    return self.retry_multi_page_as_single(
                        input,
                        options,
                        file_content,
                        *source_start_page,
                        *source_end_page,
                        *request_start_page_id,
                        *document_id,
                        run_id,
                    );
                }
            }
        }

        // 规范化为 OcrPageResult
        let results = crate::kb::ocr_response::normalize_glm_ocr_response(
            &glm_response,
            *source_start_page,
            *source_end_page,
            *request_start_page_id,
            self.name(),
            &options.model,
            Some(&request_id),
            &options.ocr_profile,
        )?;

        Ok(results)
    }
}

/// 单页请求最大重试次数
const MAX_SINGLE_PAGE_RETRIES: u32 = 3;
/// 单页请求初始退避时间（秒），每次重试翻倍
const INITIAL_SINGLE_PAGE_BACKOFF_SECS: u64 = 2;

/// 单页请求错误分类
enum SinglePageError {
    /// 可重试错误（429/503/timeout/网络）
    Retryable(String),
    /// 不可重试错误（解析失败等）
    Fatal(String),
}

impl GlmOcrProvider {
    /// 识别单张 JPG/PNG。图片请求不携带 PDF 页码范围参数。
    fn recognize_image(
        &self,
        file: &crate::kb::ocr_provider::OcrFilePayload,
        mime_type: &str,
        document_id: i64,
        run_id: &str,
        options: &OcrOptions,
    ) -> Result<Vec<OcrPageResult>> {
        let file_content = match file {
            crate::kb::ocr_provider::OcrFilePayload::Base64(data) => {
                image_base64_to_data_uri(data, mime_type)?
            }
            crate::kb::ocr_provider::OcrFilePayload::Url(url) => url.clone(),
        };
        let request_id = generate_request_id(document_id, run_id, 1, 1);
        let mut body = serde_json::json!({
            "model": options.model,
            "file": file_content,
            "enable_layout": options.enable_layout,
            "request_id": request_id,
        });
        if options.return_crop_images {
            body["return_crop_images"] = serde_json::json!(true);
        }
        if options.need_layout_visualization {
            body["need_layout_visualization"] = serde_json::json!(true);
        }

        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(
                options.timeout_seconds_per_page,
            ))
            .build()
            .map_err(|e| GBrainError::Http(format!("创建 HTTP client 失败: {}", e)))?;
        let response = client
            .post(&options.base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .map_err(|e| GBrainError::Http(format!("GLM-OCR 请求失败: {}", e)))?;
        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().unwrap_or_default();
            return Err(GBrainError::Http(format!(
                "GLM-OCR API 错误 (status={}): {}",
                status, error_text
            )));
        }

        // 先读取原始文本，反序列化失败时可包含原始响应便于排查
        let response_text = response
            .text()
            .map_err(|e| GBrainError::Serialization(format!("GLM-OCR 响应体读取失败: {}", e)))?;
        let response_json: serde_json::Value =
            serde_json::from_str(&response_text).map_err(|e| {
                let preview: String = response_text.chars().take(200).collect();
                GBrainError::Serialization(format!(
                    "GLM-OCR 响应解析失败: {}，原始响应前200字符: {}",
                    e, preview
                ))
            })?;
        let mut glm_response: GlmOcrResponse = serde_json::from_value(response_json.clone())
            .map_err(|e| {
                let preview: String = response_text.chars().take(200).collect();
                GBrainError::Serialization(format!(
                    "GLM-OCR 响应结构解析失败: {}，原始响应前200字符: {}",
                    e, preview
                ))
            })?;
        glm_response.raw_json = response_json;

        normalize_glm_ocr_response(
            &glm_response,
            1,
            1,
            1,
            self.name(),
            &options.model,
            Some(&request_id),
            &options.ocr_profile,
        )
    }

    /// 发送单页 OCR 请求并解析响应
    ///
    /// 返回 Ok(results) 表示成功，Err(SinglePageError) 表示失败（可区分是否可重试）。
    #[allow(clippy::too_many_arguments)]
    fn send_single_page_request(
        &self,
        client: &reqwest::blocking::Client,
        base_url: &str,
        body: &serde_json::Value,
        single_source_page: i32,
        single_request_page_id: i32,
        request_id: &str,
        model: &str,
        ocr_profile: &str,
    ) -> std::result::Result<Vec<OcrPageResult>, SinglePageError> {
        let resp = match client
            .post(base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(body)
            .send()
        {
            Ok(r) => r,
            Err(e) => {
                let retryable = is_reqwest_error_retryable(&e);
                let msg = format!("请求失败: {}", e);
                return if retryable {
                    Err(SinglePageError::Retryable(msg))
                } else {
                    Err(SinglePageError::Fatal(msg))
                };
            }
        };

        let status = resp.status();
        if !status.is_success() {
            let error_text = resp.text().unwrap_or_default();
            let msg = format!("API 错误 (status={}): {}", status, error_text);
            return if is_retryable_status(status) {
                Err(SinglePageError::Retryable(msg))
            } else {
                Err(SinglePageError::Fatal(msg))
            };
        }

        let resp_json: serde_json::Value = match resp.json() {
            Ok(v) => v,
            Err(e) => {
                return Err(SinglePageError::Fatal(format!("响应解析失败: {}", e)));
            }
        };

        let mut single_resp: GlmOcrResponse = match serde_json::from_value(resp_json.clone()) {
            Ok(r) => r,
            Err(e) => {
                return Err(SinglePageError::Fatal(format!("响应结构解析失败: {}", e)));
            }
        };
        single_resp.raw_json = resp_json;

        match normalize_glm_ocr_response(
            &single_resp,
            single_source_page,
            single_source_page,
            single_request_page_id,
            self.name(),
            model,
            Some(request_id),
            ocr_profile, // 使用配置的 profile，而非硬编码 "general"
        ) {
            Ok(results) => Ok(results),
            Err(e) => Err(SinglePageError::Fatal(e.to_string())),
        }
    }

    /// 多页 md_results-only 自动重试：逐页发送单页请求并聚合结果
    #[allow(clippy::too_many_arguments)]
    fn retry_multi_page_as_single(
        &self,
        _input: &OcrInput,
        options: &OcrOptions,
        file_content: String,
        source_start_page: i32,
        source_end_page: i32,
        request_start_page_id: i32,
        document_id: i64,
        run_id: &str,
    ) -> Result<Vec<OcrPageResult>> {
        tracing::warn!(
            start = source_start_page,
            end = source_end_page,
            "GLM-OCR 多页请求返回 md_results 但无 layout_details，自动拆为单页重试"
        );

        let total_pages = (source_end_page - source_start_page + 1) as usize;
        let mut all_results = Vec::with_capacity(total_pages);
        // 记录失败的页，用于在返回结果时附带错误信息
        let mut failed_pages: Vec<(i32, String)> = Vec::new();

        for page_offset in 0..total_pages {
            let single_source_page = source_start_page + page_offset as i32;
            let single_request_page_id = request_start_page_id + page_offset as i32;

            let request_id =
                generate_request_id(document_id, run_id, single_source_page, single_source_page);

            let mut body = serde_json::json!({
                "model": options.model,
                "file": file_content,
                "start_page_id": single_request_page_id,
                "end_page_id": single_request_page_id,
                "enable_layout": options.enable_layout,
                "request_id": request_id,
            });
            // 保留主请求中的调试/可视化开关，确保同一 job 结果一致
            if options.return_crop_images {
                body["return_crop_images"] = serde_json::json!(true);
            }
            if options.need_layout_visualization {
                body["need_layout_visualization"] = serde_json::json!(true);
            }

            // 指数退避重试：对可重试错误（429/503/timeout/网络）重试最多 MAX_OCR_RETRIES 次
            let mut retry_count = 0u32;
            let mut page_done = false;
            loop {
                let client = match reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_secs(
                        options.timeout_seconds_per_page,
                    ))
                    .build()
                {
                    Ok(c) => c,
                    Err(e) => {
                        // client 创建失败通常不可重试
                        failed_pages
                            .push((single_source_page, format!("创建 HTTP client 失败: {}", e)));
                        break;
                    }
                };

                match self.send_single_page_request(
                    &client,
                    &options.base_url,
                    &body,
                    single_source_page,
                    single_request_page_id,
                    &request_id,
                    &options.model,
                    &options.ocr_profile,
                ) {
                    Ok(results) => {
                        all_results.extend(results);
                        page_done = true;
                        break;
                    }
                    Err(SinglePageError::Retryable(msg)) => {
                        if retry_count < MAX_SINGLE_PAGE_RETRIES {
                            retry_count += 1;
                            let backoff_secs =
                                INITIAL_SINGLE_PAGE_BACKOFF_SECS * 2u64.pow(retry_count - 1);
                            let safe_msg = crate::kb::ocr::sanitize_error_text_with_secret(
                                &msg,
                                Some(&self.api_key),
                            );
                            tracing::warn!(
                                page = single_source_page,
                                retry = retry_count,
                                max_retries = MAX_SINGLE_PAGE_RETRIES,
                                backoff_secs,
                                error = %safe_msg,
                                "GLM-OCR 单页重试遇到可重试错误，指数退避"
                            );
                            std::thread::sleep(std::time::Duration::from_secs(backoff_secs));
                            continue;
                        }
                        failed_pages.push((single_source_page, msg));
                        break;
                    }
                    Err(SinglePageError::Fatal(msg)) => {
                        failed_pages.push((single_source_page, msg));
                        break;
                    }
                }
            }

            if !page_done {
                tracing::warn!(
                    page = single_source_page,
                    retries = retry_count,
                    "GLM-OCR 单页重试最终失败"
                );
            }
        }

        // 全部失败时返回 Err，让调用方标记整段 failed
        if all_results.is_empty() && !failed_pages.is_empty() {
            return Err(GBrainError::Http(format!(
                "GLM-OCR 单页重试全部失败: {}",
                failed_pages
                    .iter()
                    .map(|(p, e)| format!("第{}页: {}", p, e))
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }

        // 部分失败时，将失败页作为空结果嵌入，携带真实错误信息
        // 调用方 persist_ocr_page_results 会通过 _ocr_failed 标记识别并写入 failed 状态
        if !failed_pages.is_empty() {
            tracing::warn!(
                failed_pages = ?failed_pages.iter().map(|(p, _)| *p).collect::<Vec<_>>(),
                "GLM-OCR 单页重试部分页失败，成功页结果已收集，失败页携带错误信息返回"
            );
            for (page_num, error_msg) in &failed_pages {
                all_results.push(OcrPageResult {
                    page_number: *page_num,
                    text: String::new(),
                    markdown: String::new(),
                    blocks: vec![],
                    layout_visualization_url: None,
                    // 标记 _ocr_failed + 真实错误，供 persist 识别
                    raw_response_json: serde_json::json!({
                        "_ocr_failed": true,
                        "error": error_msg,
                    }),
                    request_id: None,
                    confidence: None,
                    provider: self.name().to_string(),
                    // 修复：记录实际使用的 model 而非空字符串
                    model: options.model.clone(),
                    ocr_page_width: None,
                    ocr_page_height: None,
                });
            }
        }

        Ok(all_results)
    }
}

/// 将 PDF 数据编码为 base64
pub fn pdf_to_base64(pdf_data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(pdf_data)
}

fn pdf_base64_to_data_uri(encoded_pdf: &str) -> String {
    format!("{}{}", PDF_DATA_URI_PREFIX, encoded_pdf)
}

fn image_base64_to_data_uri(encoded_image: &str, mime_type: &str) -> Result<String> {
    let prefix = match mime_type {
        "image/png" => PNG_DATA_URI_PREFIX,
        "image/jpeg" => JPEG_DATA_URI_PREFIX,
        _ => {
            return Err(GBrainError::InvalidInput(format!(
                "GLM-OCR 不支持图片 MIME 类型: {}",
                mime_type
            )))
        }
    };
    Ok(format!("{}{}", prefix, encoded_image))
}

/// 从 URL 中移除可能包含凭证的敏感部分（userinfo、query、fragment），
/// 仅保留 `scheme://host:port/path`，用于审计日志脱敏
fn sanitize_url_for_log(url: &str) -> String {
    // 1. 移除 fragment（# 之后）
    let url = url.split('#').next().unwrap_or(url);
    // 2. 移除 query string（? 之后）
    let url = url.split('?').next().unwrap_or(url);
    // 3. 移除 userinfo（:// 之后、@ 之前的部分）
    if let Some(scheme_end) = url.find("://") {
        let after_scheme = &url[scheme_end + 3..];
        if let Some(at_pos) = after_scheme.find('@') {
            let scheme_part = &url[..scheme_end + 3];
            let host_and_path = &after_scheme[at_pos + 1..];
            return format!("{}{}", scheme_part, host_and_path);
        }
    }
    url.to_string()
}

/// 从环境配置构建 OcrOptions
pub fn build_ocr_options_from_config(config: &crate::config::Config) -> OcrOptions {
    // 当 ocr_allow_custom_base_url=false 时，忽略用户配置的 base_url，
    // 强制使用官方默认端点，防止数据泄露到非授权服务器
    let default_url = "https://open.bigmodel.cn/api/paas/v4/layout_parsing";
    let base_url = if config.ocr_allow_custom_base_url {
        let url = config.ocr_base_url.clone();
        // 仅在实际启用非默认 endpoint 时记录不含密钥的审计日志，
        // 移除 userinfo、query、fragment 防止凭证泄露
        if url != default_url {
            tracing::warn!(
                "审计: OCR 使用自定义 endpoint (ocr_allow_custom_base_url=true)，数据将发送至非官方服务器: {}",
                sanitize_url_for_log(&url)
            );
        }
        url
    } else {
        default_url.to_string()
    };

    OcrOptions {
        model: config.ocr_model.clone(),
        base_url,
        timeout_seconds_per_page: config.ocr_timeout_seconds_per_page,
        mode: crate::kb::ocr_provider::OcrMode::from_str(&config.ocr_mode),
        submit_mode: crate::kb::ocr_provider::OcrSubmitMode::from_str(&config.ocr_submit_mode),
        enable_layout: config.ocr_enable_layout,
        return_crop_images: config.ocr_return_crop_images,
        need_layout_visualization: config.ocr_need_layout_visualization,
        max_pages_per_request: config.ocr_max_pages_per_request,
        max_pdf_bytes_per_request: config.ocr_max_pdf_bytes_per_request,
        ocr_profile: config.ocr_profile.clone(),
    }
}

/// 判断 reqwest 错误是否可重试。
fn is_reqwest_error_retryable(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.status().is_some_and(is_retryable_status)
}

/// 判断 HTTP 状态码是否可重试。
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::BAD_GATEWAY
        || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        || status == reqwest::StatusCode::GATEWAY_TIMEOUT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_ocr_options() {
        let config = crate::config::Config::default();
        let options = build_ocr_options_from_config(&config);
        assert_eq!(options.model, "glm-ocr");
        assert!(options.enable_layout);
        // 默认每次 OCR 请求最多 5 页，避免单次 GLM-OCR 请求过大导致超时或失败。
        assert_eq!(options.max_pages_per_request, 5);
        assert_eq!(options.max_pdf_bytes_per_request, 52_428_800);
    }

    #[test]
    fn recognize_off_async_context_survives_block_on() {
        // 回归：在 rt.block_on 内直接调 recognize（reqwest::blocking）会 panic
        // "Cannot drop a runtime in a context where blocking is not allowed"，
        // 曾导致 DOCX 内嵌图片 OCR 静默杀死 kb-worker 线程、job 永久卡 running。
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build runtime");

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rt.block_on(async {
                let provider = GlmOcrProvider::new("test-key");
                let mut options = build_ocr_options_from_config(&crate::config::Config::default());
                options.base_url = "http://127.0.0.1:9/".to_string(); // 不可路由，快速失败
                options.timeout_seconds_per_page = 2;
                let input = OcrInput::Image {
                    file: crate::kb::ocr_provider::OcrFilePayload::Base64("aGVsbG8=".to_string()),
                    mime_type: "image/png".to_string(),
                    document_id: -1,
                    run_id: "regression-test".to_string(),
                };
                provider.recognize_off_async_context(&input, &options)
            })
        }))
        .expect("recognize_off_async_context 在 block_on 内不得 panic");

        assert!(result.is_err(), "不可路由端点应返回 Err 而非 Ok");
    }

    #[test]
    fn test_pdf_to_base64() {
        let data = b"Hello PDF";
        let encoded = pdf_to_base64(data);
        assert!(!encoded.is_empty());
        // base64 编码后长度应为 4/3 倍（向上取整到 4 的倍数）
        assert_eq!(encoded.len() % 4, 0);
    }

    #[test]
    fn test_pdf_base64_is_wrapped_as_maas_data_uri() {
        use base64::Engine;

        let data = b"%PDF-1.7\nfixture";
        let file_content = pdf_base64_to_data_uri(&pdf_to_base64(data));
        let encoded = file_content
            .strip_prefix(PDF_DATA_URI_PREFIX)
            .expect("PDF request payload must use a data URI");

        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .expect("data URI should contain valid base64"),
            data
        );
    }

    #[test]
    fn test_image_base64_is_wrapped_with_supported_mime() {
        assert_eq!(
            image_base64_to_data_uri("YWJj", "image/png").unwrap(),
            "data:image/png;base64,YWJj"
        );
        assert_eq!(
            image_base64_to_data_uri("YWJj", "image/jpeg").unwrap(),
            "data:image/jpeg;base64,YWJj"
        );
        assert!(image_base64_to_data_uri("YWJj", "image/webp").is_err());
    }
}
