//! Rendering for the terminal.
//!
//! Two rules run through everything here:
//!
//! - Structured output goes to stdout; logs go to stderr. `cuma usage --json`
//!   must be pipeable into `jq` with the log level turned up.
//! - A number that was estimated is never printed as though it were measured.

use cuma_usage::UsageTotals;

/// A simple aligned table.
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    /// A table with these column headers.
    pub fn new(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|h| (*h).to_owned()).collect(),
            rows: Vec::new(),
        }
    }

    /// Append a row. Rows shorter than the header are padded.
    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    /// Render the table.
    pub fn render(&self) -> String {
        let column_count = self.headers.len();

        let mut widths: Vec<usize> = self.headers.iter().map(|h| h.chars().count()).collect();
        for row in &self.rows {
            for (index, cell) in row.iter().take(column_count).enumerate() {
                widths[index] = widths[index].max(cell.chars().count());
            }
        }

        let mut out = String::new();

        for (index, header) in self.headers.iter().enumerate() {
            out.push_str(&pad(header, widths[index]));
            if index + 1 < column_count {
                out.push_str("  ");
            }
        }
        out.push('\n');

        let rule: usize = widths.iter().sum::<usize>() + 2 * column_count.saturating_sub(1);
        out.push_str(&"-".repeat(rule));
        out.push('\n');

        for row in &self.rows {
            for (index, width) in widths.iter().enumerate() {
                let cell = row.get(index).map(String::as_str).unwrap_or("");
                out.push_str(&pad(cell, *width));
                if index + 1 < column_count {
                    out.push_str("  ");
                }
            }
            out.push('\n');
        }

        out
    }
}

fn pad(text: &str, width: usize) -> String {
    let length = text.chars().count();
    if length >= width {
        return text.to_owned();
    }
    format!("{text}{}", " ".repeat(width - length))
}

/// Render a success rate, or `-` when nothing has run.
pub fn render_rate(rate: Option<f64>) -> String {
    rate.map_or_else(|| "-".to_owned(), |r| format!("{:.0}%", r * 100.0))
}

/// Render a latency, or `-`.
pub fn render_latency(ms: Option<u64>) -> String {
    match ms {
        None => "-".to_owned(),
        Some(ms) if ms < 1000 => format!("{ms}ms"),
        Some(ms) => format!("{:.1}s", ms as f64 / 1000.0),
    }
}

/// Render a token count compactly.
pub fn render_tokens(tokens: u64) -> String {
    match tokens {
        0 => "-".to_owned(),
        t if t < 1_000 => t.to_string(),
        t if t < 1_000_000 => format!("{:.1}K", t as f64 / 1_000.0),
        t => format!("{:.1}M", t as f64 / 1_000_000.0),
    }
}

/// One row of a usage table.
pub fn usage_row(label: &str, totals: &UsageTotals) -> Vec<String> {
    vec![
        label.to_owned(),
        totals.attempts.to_string(),
        render_rate(totals.success_rate()),
        render_tokens(totals.total_tokens()),
        // `render_cost` carries the estimate marker, so a partly-unpriced
        // total cannot be mistaken for a measured one.
        totals.render_cost(),
        render_latency(totals.mean_latency_ms()),
    ]
}

/// The header for a usage table.
pub const USAGE_HEADERS: &[&str] = &["Name", "Tasks", "Success", "Tokens", "Cost", "Mean latency"];

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn a_table_aligns_its_columns_to_the_widest_cell() {
        let mut table = Table::new(&["Agent", "Tasks"]);
        table.row(vec!["a-very-long-agent-name".into(), "1".into()]);
        table.row(vec!["b".into(), "22".into()]);

        let rendered = table.render();
        let lines: Vec<&str> = rendered.lines().collect();

        // Header, rule, two rows.
        assert_eq!(lines.len(), 4);
        assert!(lines[1].starts_with("---"));
        assert!(lines[3].starts_with("b  "), "short names must be padded");
    }

    #[test]
    fn a_short_row_is_padded_rather_than_panicking() {
        let mut table = Table::new(&["A", "B", "C"]);
        table.row(vec!["only-one".into()]);
        assert!(table.render().contains("only-one"));
    }

    #[test]
    fn an_empty_table_still_renders_its_header() {
        assert!(Table::new(&["Agent"]).render().contains("Agent"));
    }

    #[test]
    fn token_counts_are_rendered_compactly() {
        assert_eq!(render_tokens(0), "-");
        assert_eq!(render_tokens(500), "500");
        assert_eq!(render_tokens(1_200), "1.2K");
        assert_eq!(render_tokens(2_100_000), "2.1M");
    }

    #[test]
    fn latencies_switch_units_at_a_second() {
        assert_eq!(render_latency(None), "-");
        assert_eq!(render_latency(Some(250)), "250ms");
        assert_eq!(render_latency(Some(1_500)), "1.5s");
    }

    #[test]
    fn an_absent_rate_renders_as_a_dash_not_as_zero_percent() {
        assert_eq!(render_rate(None), "-");
        assert_eq!(render_rate(Some(0.0)), "0%");
        assert_eq!(render_rate(Some(0.923)), "92%");
    }

    #[test]
    fn a_usage_row_marks_an_unpriced_total_as_unknown() {
        let totals = UsageTotals {
            attempts: 3,
            successes: 3,
            attempts_without_pricing: 3,
            ..UsageTotals::default()
        };

        let row = usage_row("mystery-agent", &totals);
        assert_eq!(row[4], "unknown", "row was {row:?}");
    }

    #[test]
    fn a_usage_row_marks_a_partly_priced_total_as_a_lower_bound() {
        let totals = UsageTotals {
            attempts: 3,
            estimated_cost_usd: 1.25,
            attempts_without_pricing: 1,
            ..UsageTotals::default()
        };

        assert!(usage_row("agent", &totals)[4].starts_with('≥'));
    }
}
