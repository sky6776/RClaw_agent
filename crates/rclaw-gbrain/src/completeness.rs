//! Entity page quality scorer — 7 rubrics with weighted dimensions.
//! Mirrors gbrain's src/core/enrichment/completeness.ts

use crate::types::*;
use std::collections::HashMap;
use std::sync::OnceLock;

static SOURCE_URL_RE: OnceLock<regex::Regex> = OnceLock::new();
static CITATION_RE: OnceLock<regex::Regex> = OnceLock::new();

/// A single scoring dimension
pub struct CompletenessDimension {
    pub name: &'static str,
    pub weight: f64,
    pub check: Box<dyn Fn(&Page) -> f64 + Send + Sync>,
}

/// A rubric for scoring a specific entity type
pub struct Rubric {
    pub entity_type: Option<PageType>, // None = default
    pub dimensions: Vec<CompletenessDimension>,
}

/// Completeness score for a page
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompletenessScore {
    pub slug: String,
    pub entity_type: String,
    pub score: f64,
    pub dimension_scores: HashMap<String, f64>,
}

impl CompletenessScore {
    /// Maximum possible score
    pub fn max() -> f64 {
        1.0
    }
}

// ── Shared dimension helpers ────────────────────────────

fn has_source_urls(page: &Page) -> f64 {
    let body = &page.compiled_truth;
    let re = SOURCE_URL_RE.get_or_init(|| regex::Regex::new(r"\]\(\s*https?://").unwrap());
    let count = re.find_iter(body).count();
    if count == 0 {
        0.0
    } else if count == 1 {
        0.6
    } else {
        1.0
    }
}

fn has_citations(page: &Page) -> f64 {
    let body = &page.compiled_truth;
    let re = CITATION_RE.get_or_init(|| {
        regex::Regex::new(r"\[Source:\s*\S[^\]]*\]|\]\(\s*https?://[^)]+\)").unwrap()
    });
    let count = re.find_iter(body).count();
    (count as f64 / 3.0).min(1.0)
}

fn has_title(page: &Page) -> f64 {
    if page.title.trim().is_empty() {
        0.0
    } else {
        1.0
    }
}

fn has_body(page: &Page) -> f64 {
    if page.compiled_truth.trim().len() > 50 {
        1.0
    } else {
        0.0
    }
}

fn recency_score(page: &Page) -> f64 {
    let d = chrono::NaiveDateTime::parse_from_str(&page.updated_at, "%Y-%m-%d %H:%M:%S")
        .ok()
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(&page.updated_at, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        });
    if let Some(d) = d {
        let age_days = (chrono::Utc::now().naive_utc() - d).num_days().max(0) as f64;
        if age_days < 90.0 {
            1.0
        } else if age_days < 180.0 {
            0.7
        } else if age_days < 365.0 {
            0.4
        } else {
            0.1
        }
    } else {
        0.0
    }
}

fn has_timeline_entries(page: &Page) -> f64 {
    let count = page
        .timeline
        .as_deref()
        .unwrap_or("")
        .lines()
        .filter(|l| l.trim().starts_with('-'))
        .count();
    if count == 0 {
        0.0
    } else if count < 3 {
        0.5
    } else {
        1.0
    }
}

fn has_frontmatter_field(page: &Page, field: &str) -> f64 {
    page.frontmatter.as_deref().map_or(0.0, |fm| {
        if fm.contains(&format!("\"{}\":", field)) || fm.contains(&format!("{}:", field)) {
            1.0
        } else {
            0.0
        }
    })
}

// ── 7 Rubrics ───────────────────────────────────────────

fn person_rubric() -> Rubric {
    Rubric {
        entity_type: Some(PageType::Person),
        dimensions: vec![
            CompletenessDimension {
                name: "has_role_and_company",
                weight: 0.20,
                check: Box::new(|p| {
                    has_frontmatter_field(p, "company").max(has_frontmatter_field(p, "role"))
                }),
            },
            CompletenessDimension {
                name: "has_source_urls",
                weight: 0.20,
                check: Box::new(has_source_urls),
            },
            CompletenessDimension {
                name: "has_timeline_entries",
                weight: 0.15,
                check: Box::new(has_timeline_entries),
            },
            CompletenessDimension {
                name: "has_citations",
                weight: 0.15,
                check: Box::new(has_citations),
            },
            CompletenessDimension {
                name: "has_backlinks",
                weight: 0.10,
                check: Box::new(|_p| 0.0),
            }, // requires engine, stub
            CompletenessDimension {
                name: "recency_score",
                weight: 0.10,
                check: Box::new(recency_score),
            },
            CompletenessDimension {
                name: "non_redundancy",
                weight: 0.10,
                check: Box::new(|_p| 1.0),
            }, // stub
        ],
    }
}

fn company_rubric() -> Rubric {
    Rubric {
        entity_type: Some(PageType::Company),
        dimensions: vec![
            CompletenessDimension {
                name: "has_description",
                weight: 0.20,
                check: Box::new(has_body),
            },
            CompletenessDimension {
                name: "has_founders",
                weight: 0.15,
                check: Box::new(|p| has_frontmatter_field(p, "founders")),
            },
            CompletenessDimension {
                name: "has_funding",
                weight: 0.15,
                check: Box::new(|p| has_frontmatter_field(p, "funding")),
            },
            CompletenessDimension {
                name: "has_source_urls",
                weight: 0.15,
                check: Box::new(has_source_urls),
            },
            CompletenessDimension {
                name: "has_citations",
                weight: 0.15,
                check: Box::new(has_citations),
            },
            CompletenessDimension {
                name: "has_employees_or_investors",
                weight: 0.10,
                check: Box::new(|p| {
                    has_frontmatter_field(p, "employees").max(has_frontmatter_field(p, "investors"))
                }),
            },
            CompletenessDimension {
                name: "recency_score",
                weight: 0.10,
                check: Box::new(recency_score),
            },
        ],
    }
}

fn project_rubric() -> Rubric {
    Rubric {
        entity_type: Some(PageType::Project),
        dimensions: vec![
            CompletenessDimension {
                name: "has_description",
                weight: 0.25,
                check: Box::new(has_body),
            },
            CompletenessDimension {
                name: "has_owners",
                weight: 0.20,
                check: Box::new(|p| has_frontmatter_field(p, "owner")),
            },
            CompletenessDimension {
                name: "has_timeline",
                weight: 0.15,
                check: Box::new(has_timeline_entries),
            },
            CompletenessDimension {
                name: "has_citations",
                weight: 0.15,
                check: Box::new(has_citations),
            },
            CompletenessDimension {
                name: "has_status",
                weight: 0.15,
                check: Box::new(|p| has_frontmatter_field(p, "status")),
            },
            CompletenessDimension {
                name: "recency_score",
                weight: 0.10,
                check: Box::new(recency_score),
            },
        ],
    }
}

fn deal_rubric() -> Rubric {
    Rubric {
        entity_type: Some(PageType::Deal),
        dimensions: vec![
            CompletenessDimension {
                name: "has_company",
                weight: 0.25,
                check: Box::new(|p| has_frontmatter_field(p, "company")),
            },
            CompletenessDimension {
                name: "has_terms",
                weight: 0.25,
                check: Box::new(|p| {
                    has_frontmatter_field(p, "amount").max(has_frontmatter_field(p, "terms"))
                }),
            },
            CompletenessDimension {
                name: "has_date",
                weight: 0.15,
                check: Box::new(|p| has_frontmatter_field(p, "date")),
            },
            CompletenessDimension {
                name: "has_source_urls",
                weight: 0.15,
                check: Box::new(has_source_urls),
            },
            CompletenessDimension {
                name: "has_citations",
                weight: 0.20,
                check: Box::new(has_citations),
            },
        ],
    }
}

fn concept_rubric() -> Rubric {
    Rubric {
        entity_type: Some(PageType::Concept),
        dimensions: vec![
            CompletenessDimension {
                name: "has_definition",
                weight: 0.35,
                check: Box::new(has_body),
            },
            CompletenessDimension {
                name: "has_citations",
                weight: 0.30,
                check: Box::new(has_citations),
            },
            CompletenessDimension {
                name: "has_examples",
                weight: 0.20,
                check: Box::new(|_p| 0.0),
            },
            CompletenessDimension {
                name: "has_related",
                weight: 0.15,
                check: Box::new(|p| has_frontmatter_field(p, "related")),
            },
        ],
    }
}

fn source_rubric() -> Rubric {
    Rubric {
        entity_type: Some(PageType::Source),
        dimensions: vec![
            CompletenessDimension {
                name: "has_url",
                weight: 0.35,
                check: Box::new(has_source_urls),
            },
            CompletenessDimension {
                name: "has_author",
                weight: 0.20,
                check: Box::new(|p| has_frontmatter_field(p, "author")),
            },
            CompletenessDimension {
                name: "has_date",
                weight: 0.20,
                check: Box::new(|p| has_frontmatter_field(p, "date")),
            },
            CompletenessDimension {
                name: "has_summary",
                weight: 0.25,
                check: Box::new(has_body),
            },
        ],
    }
}

fn default_rubric() -> Rubric {
    Rubric {
        entity_type: None,
        dimensions: vec![
            CompletenessDimension {
                name: "has_title",
                weight: 0.30,
                check: Box::new(has_title),
            },
            CompletenessDimension {
                name: "has_content",
                weight: 0.30,
                check: Box::new(has_body),
            },
            CompletenessDimension {
                name: "has_source_urls",
                weight: 0.20,
                check: Box::new(has_source_urls),
            },
            CompletenessDimension {
                name: "has_citations",
                weight: 0.20,
                check: Box::new(has_citations),
            },
        ],
    }
}

/// Get the rubric for a specific page type, falling back to default
fn get_rubric_for_type(page_type: &PageType) -> &Rubric {
    // Static rubrics to avoid Clone issues with Box<dyn Fn>
    use std::sync::OnceLock;
    static PERSON: OnceLock<Rubric> = OnceLock::new();
    static COMPANY: OnceLock<Rubric> = OnceLock::new();
    static PROJECT: OnceLock<Rubric> = OnceLock::new();
    static DEAL: OnceLock<Rubric> = OnceLock::new();
    static CONCEPT: OnceLock<Rubric> = OnceLock::new();
    static SOURCE: OnceLock<Rubric> = OnceLock::new();
    static DEFAULT: OnceLock<Rubric> = OnceLock::new();

    let rubric: &Rubric = match page_type {
        PageType::Person => PERSON.get_or_init(person_rubric),
        PageType::Company => COMPANY.get_or_init(company_rubric),
        PageType::Project => PROJECT.get_or_init(project_rubric),
        PageType::Deal => DEAL.get_or_init(deal_rubric),
        PageType::Concept => CONCEPT.get_or_init(concept_rubric),
        PageType::Source => SOURCE.get_or_init(source_rubric),
        _ => DEFAULT.get_or_init(default_rubric),
    };
    rubric
}

/// Score a single page against its entity-type rubric
pub fn score_page(page: &Page) -> CompletenessScore {
    let rubric = get_rubric_for_type(&page.page_type);
    compute_score(page, rubric)
}

fn compute_score(page: &Page, rubric: &Rubric) -> CompletenessScore {
    let mut dimension_scores = HashMap::new();
    let mut total = 0.0f64;

    for dim in &rubric.dimensions {
        let raw = (dim.check)(page).clamp(0.0, 1.0);
        let weighted = raw * dim.weight;
        dimension_scores.insert(dim.name.to_string(), raw);
        total += weighted;
    }

    CompletenessScore {
        slug: page.slug.clone(),
        entity_type: page.page_type.to_string(),
        score: (total * 1000.0).round() / 1000.0,
        dimension_scores,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_page() -> Page {
        Page {
            id: 1,
            slug: "people/alice".into(),
            page_type: PageType::Person,
            title: "Alice".into(),
            compiled_truth: "Engineer at Acme. [Source: [X/alice](https://x.com/alice/status/1)]"
                .into(),
            timeline: Some("- **2024-01** | Started\n- **2024-06** | Promoted".into()),
            frontmatter: Some(r#"{"company":"Acme","role":"Engineer"}"#.into()),
            content_hash: None,
            created_at: "2024-01-01".into(),
            updated_at: "2024-06-15".into(),
            deleted_at: None,
        }
    }

    #[test]
    fn test_score_person_page() {
        let score = score_page(&test_page());
        assert!(score.score > 0.0);
        assert!(score.score <= 1.0);
        assert_eq!(score.entity_type, "person");
    }

    #[test]
    fn test_score_default_rubric() {
        let mut p = test_page();
        p.page_type = PageType::Note;
        let score = score_page(&p);
        assert!(score.score >= 0.0);
    }
}
