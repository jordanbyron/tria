//! Findings an agent reported with Claude Code's `ReportFindings`, drawn as a list.
//!
//! A code review hands its findings to the host as the input of that call rather than
//! writing them into the reply, which only points at them. A tool row draws that input
//! as a line of JSON, so the findings are read out of it and drawn where they were
//! reported, the way a reader expects to find what the reply points at.

use serde_json::Value;

use crate::model::Activity;

/// One finding, as the call reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub file: String,
    pub line: Option<u64>,
    pub category: Option<String>,
    pub short_summary: Option<String>,
    pub summary: String,
    pub failure_scenario: Option<String>,
    pub verdict: Option<String>,
    pub outcome: Option<String>,
}

/// The findings a finished call reported, if it is a `ReportFindings`.
pub fn of(activity: &Activity) -> Option<Vec<Finding>> {
    if activity.kind != "tool.completed" {
        return None;
    }
    let data = activity.payload.get("data")?;
    if data.get("toolName").and_then(Value::as_str)? != "ReportFindings" {
        return None;
    }
    let findings = data.pointer("/input/findings")?.as_array()?;
    Some(findings.iter().filter_map(finding).collect())
}

fn finding(value: &Value) -> Option<Finding> {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    Some(Finding {
        file: text("file")?,
        line: value
            .get("line")
            .and_then(Value::as_u64)
            .filter(|&line| line > 0),
        category: text("category"),
        short_summary: text("short_summary"),
        summary: text("summary")?,
        failure_scenario: text("failure_scenario"),
        verdict: text("verdict"),
        outcome: text("outcome"),
    })
}

/// The findings as Markdown, most severe first as the call ordered them.
pub fn markdown(findings: &[Finding]) -> String {
    let mut out = String::new();
    for (index, finding) in findings.iter().enumerate() {
        let place = match finding.line {
            Some(line) => format!("{}:{line}", finding.file),
            None => finding.file.clone(),
        };
        let headline = finding.short_summary.as_deref().unwrap_or(&finding.summary);
        out.push_str(&format!("**{}. {headline}**  \n`{place}`", index + 1));
        for tag in [&finding.category, &finding.verdict, &finding.outcome]
            .into_iter()
            .flatten()
        {
            out.push_str(&format!(" · *{tag}*"));
        }
        out.push_str("\n\n");
        if finding.short_summary.is_some() {
            out.push_str(&format!("{}\n\n", finding.summary));
        }
        if let Some(scenario) = &finding.failure_scenario {
            out.push_str(&format!("*When:* {scenario}\n\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn activity(kind: &str, data: Value) -> Activity {
        serde_json::from_value(json!({
            "id": "a1", "kind": kind, "payload": { "data": data },
        }))
        .unwrap()
    }

    #[test]
    fn findings_are_read_from_the_input_of_the_call() {
        let found = of(&activity(
            "tool.completed",
            json!({
                "toolName": "ReportFindings",
                "input": { "findings": [
                    {
                        "file": "src/a.rs", "line": 12, "category": "correctness",
                        "short_summary": "Off by one", "summary": "The loop stops early.",
                        "failure_scenario": "Three items → two are read.",
                    },
                    { "file": "src/b.rs", "summary": "Unused import." },
                    { "summary": "No file, so not a finding." },
                ]},
                "result": { "content": null },
            }),
        ))
        .unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].line, Some(12));
        assert_eq!(found[0].short_summary.as_deref(), Some("Off by one"));
        assert_eq!(found[1].line, None);
        assert_eq!(found[1].category, None);
    }

    #[test]
    fn only_a_finished_report_has_findings() {
        let data = json!({ "toolName": "ReportFindings", "input": { "findings": [] } });
        assert_eq!(of(&activity("tool.started", data.clone())), None);
        assert_eq!(of(&activity("tool.completed", data)), Some(Vec::new()));
        let other = json!({ "toolName": "Read", "input": { "findings": [] } });
        assert_eq!(of(&activity("tool.completed", other)), None);
    }

    #[test]
    fn a_finding_reads_headline_place_and_scenario() {
        let text = markdown(&[Finding {
            file: "src/a.rs".into(),
            line: Some(12),
            category: Some("correctness".into()),
            short_summary: Some("Off by one".into()),
            summary: "The loop stops early.".into(),
            failure_scenario: Some("Three items → two are read.".into()),
            verdict: None,
            outcome: None,
        }]);
        assert!(text.starts_with("**1. Off by one**  \n`src/a.rs:12` · *correctness*"));
        assert!(text.contains("The loop stops early."));
        assert!(text.contains("*When:* Three items → two are read."));
    }
}
