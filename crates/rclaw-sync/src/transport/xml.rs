//! 网络后端解析服务端 XML（WebDAV 的 multistatus、S3 的列表结果）用的小工具。
//!
//! 只看元素的本地名，不管命名空间前缀：各家服务端用的前缀不一样（`D:`、`d:`、无前缀）。

use quick_xml::events::Event;
use quick_xml::Reader;

use super::TransportResult;
use crate::error::TransportError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum XmlEvent {
    Start(String),
    End(String),
    Text(String),
}

/// 服务端 XML 响应的长度上限（目录列表）。
pub(crate) const XML_MAX: usize = 16 * 1024 * 1024;
/// 报错回应的长度上限：只从里面取一个错误码。
pub(crate) const XML_ERROR_MAX: usize = 64 * 1024;

fn local_name(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).to_ascii_lowercase()
}

/// 把 XML 从头到尾走一遍，每遇到一个事件叫一次 `visit`。元素名一律转成小写的本地名；
/// 相邻的文本并成一段、去掉首尾空白，空白文本丢弃。
///
/// 不把整份文档摊成列表：服务端不可信，一份十几兆、全是空元素的回应摊开要占几百兆内存。
/// 这里用到的内存只有当前这一段文本，剩下的由调用方按自己要留的东西决定。
pub(crate) fn xml_walk(
    xml: &str,
    mut visit: impl FnMut(XmlEvent) -> TransportResult<()>,
) -> TransportResult<()> {
    if xml.len() > XML_MAX {
        return Err(TransportError::Other("服务端返回的 XML 过大".into()));
    }
    let bad = |e: &dyn std::fmt::Display| {
        TransportError::Other(format!("服务端返回的 XML 无法解析: {e}"))
    };
    fn gather(pending: &mut String, piece: &str) {
        if !piece.trim().is_empty() {
            pending.push_str(piece);
        }
    }
    // 攒着的文本：到下一个元素的开头或结尾（或文档结束）才交出去。
    fn flush(
        pending: &mut String,
        visit: &mut impl FnMut(XmlEvent) -> TransportResult<()>,
    ) -> TransportResult<()> {
        if pending.is_empty() {
            return Ok(());
        }
        let text = pending.trim().to_string();
        pending.clear();
        visit(XmlEvent::Text(text))
    }
    let mut reader = Reader::from_str(xml);
    let mut pending = String::new();
    // 还开着的元素个数。文档结束时不是零，说明回应是半截的：不能把它当成完整的列表。
    let mut open = 0usize;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                flush(&mut pending, &mut visit)?;
                open += 1;
                visit(XmlEvent::Start(local_name(e.local_name().as_ref())))?;
            }
            Ok(Event::Empty(e)) => {
                flush(&mut pending, &mut visit)?;
                let name = local_name(e.local_name().as_ref());
                visit(XmlEvent::Start(name.clone()))?;
                visit(XmlEvent::End(name))?;
            }
            Ok(Event::End(e)) => {
                flush(&mut pending, &mut visit)?;
                open = open.saturating_sub(1);
                visit(XmlEvent::End(local_name(e.local_name().as_ref())))?;
            }
            Ok(Event::Text(t)) => {
                let text = t.unescape().map_err(|e| bad(&e))?;
                gather(&mut pending, &text);
            }
            Ok(Event::CData(c)) => {
                let raw = c.into_inner();
                gather(&mut pending, &String::from_utf8_lossy(&raw));
            }
            Ok(Event::Eof) => {
                if open != 0 {
                    return Err(bad(&"文档不完整（有元素没有结束）"));
                }
                return flush(&mut pending, &mut visit);
            }
            Ok(_) => {}
            Err(e) => return Err(bad(&e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xml_events(xml: &str) -> TransportResult<Vec<XmlEvent>> {
        let mut out = Vec::new();
        xml_walk(xml, |event| {
            out.push(event);
            Ok(())
        })?;
        Ok(out)
    }

    #[test]
    fn flattens_with_local_names() {
        let events = xml_events(
            r#"<?xml version="1.0"?><D:a xmlns:D="DAV:"><D:B>x &amp; y</D:B><c/>  </D:a>"#,
        )
        .unwrap();
        assert_eq!(
            events,
            vec![
                XmlEvent::Start("a".into()),
                XmlEvent::Start("b".into()),
                XmlEvent::Text("x & y".into()),
                XmlEvent::End("b".into()),
                XmlEvent::Start("c".into()),
                XmlEvent::End("c".into()),
                XmlEvent::End("a".into()),
            ]
        );
    }

    #[test]
    fn adjacent_text_is_joined_and_trimmed() {
        let events = xml_events("<a>  one <!-- 注释 --><![CDATA[two]]>  </a><b>   </b>").unwrap();
        assert_eq!(
            events,
            vec![
                XmlEvent::Start("a".into()),
                XmlEvent::Text("one two".into()),
                XmlEvent::End("a".into()),
                XmlEvent::Start("b".into()),
                XmlEvent::End("b".into()),
            ]
        );
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(xml_events("<a><b></a>").is_err());
        assert!(xml_events("").unwrap().is_empty());
        assert!(xml_events("plain text").is_ok());
    }

    #[test]
    fn a_document_cut_short_is_an_error_not_a_shorter_document() {
        // 半截的回应：不能被读成一份完整的、只是短一些的列表。
        assert!(xml_events("<list><item>a</item><item>b</item>").is_err());
        assert!(xml_events("<list><item>a</item><item>b").is_err());
        assert!(xml_events("<list><item>a</item><item>b</item></list>").is_ok());
    }

    #[test]
    fn the_walk_stops_as_soon_as_the_visitor_refuses() {
        let mut seen = 0;
        let outcome = xml_walk(&"<a/>".repeat(1000), |_| {
            seen += 1;
            if seen == 10 {
                Err(TransportError::Other("够了".into()))
            } else {
                Ok(())
            }
        });
        assert!(outcome.is_err());
        assert_eq!(seen, 10);
    }
}
