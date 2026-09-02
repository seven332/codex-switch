use std::io::IsTerminal;

use crate::usage_forecast::ForecastUnavailableReason;

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_BOLD_GREEN: &str = "\x1b[1;32m";
const ANSI_BOLD_RED: &str = "\x1b[1;31m";
const QUOTA_BAR_FILLED_CHAR: char = '\u{2588}';

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct UsageOutputStyle {
    enabled: bool,
}

impl UsageOutputStyle {
    fn plain() -> Self {
        Self { enabled: false }
    }

    fn colored() -> Self {
        Self { enabled: true }
    }

    pub(crate) fn style_text(self, text: &str) -> String {
        if !self.enabled {
            return text.to_string();
        }

        let mut styled = String::with_capacity(text.len());
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                styled.push('\n');
            }
            styled.push_str(&self.style_line(line));
        }
        styled
    }

    fn style_line(self, line: &str) -> String {
        if line.is_empty() {
            return String::new();
        }

        if line.starts_with("rate-limit resets:") {
            return if line.ends_with(", expiring soon") {
                self.wrap(line, ANSI_YELLOW)
            } else {
                line.to_string()
            };
        }
        if is_usage_account_header_line(line) {
            return self.style_account_header_line(line);
        }
        if line.starts_with("status:") || line.starts_with("usage error:") {
            return self.wrap(line, ANSI_RED);
        }
        if line == "overall estimate:" {
            return self.wrap(line, ANSI_BOLD);
        }
        if let Some(line) = self.style_forecast_line(line) {
            return line;
        }
        if line.starts_with("additional ") {
            return self.wrap(line, ANSI_BOLD);
        }
        if let Some(line) = self.style_quota_line(line) {
            return line;
        }
        if line.contains("└ reset") {
            return line.to_string();
        }
        if line.starts_with("plan:")
            || line.starts_with("usage: unsupported")
            || line.starts_with("  ")
        {
            return self.wrap(line, ANSI_DIM);
        }

        line.to_string()
    }

    fn style_forecast_line(self, line: &str) -> Option<String> {
        if line.starts_with("  unavailable: not expected") {
            return Some(self.wrap(line, ANSI_GREEN));
        }
        if line.strip_prefix("  unavailable: ").is_some_and(|reason| {
            ForecastUnavailableReason::ALL
                .into_iter()
                .any(|candidate| candidate.as_str() == reason)
        }) {
            return None;
        }
        if line.starts_with("  unavailable:") {
            return Some(self.wrap(line, ANSI_YELLOW));
        }
        None
    }

    fn style_account_header_line(self, line: &str) -> String {
        let (line, unavailable) = line
            .strip_suffix(" [UNAVAILABLE]")
            .map_or((line, false), |line| (line, true));
        let mut styled = String::new();

        if let Some(rest) = line.strip_prefix("* ") {
            styled.push_str(&self.wrap("*", ANSI_BOLD_GREEN));
            styled.push(' ');
            styled.push_str(&self.wrap(rest, ANSI_BOLD));
        } else {
            styled.push_str(&self.wrap(line, ANSI_BOLD));
        }

        if unavailable {
            styled.push(' ');
            styled.push_str(&self.wrap("[UNAVAILABLE]", ANSI_BOLD_RED));
        }

        styled
    }

    fn style_quota_line(self, line: &str) -> Option<String> {
        let marker = "┬ quota [";
        let marker_start = line.find(marker)?;
        let bar_start = marker_start + marker.len();
        let after_bar_start = &line[bar_start..];
        let bar_end = after_bar_start.find(']')?;
        let bar = &after_bar_start[..bar_end];
        let suffix = &after_bar_start[bar_end + 1..];
        let left_percent = parse_usage_left_percent(suffix.trim_start())?;
        let risk_style = usage_left_risk_style(left_percent);

        let mut styled = String::new();
        styled.push_str(&line[..bar_start]);
        styled.push_str(&style_quota_bar(bar, risk_style));
        styled.push(']');

        let leading_spaces = suffix.len() - suffix.trim_start().len();
        styled.push_str(&" ".repeat(leading_spaces));
        styled.push_str(&style_usage_left_text(suffix.trim_start(), risk_style));

        Some(styled)
    }

    fn wrap(self, line: &str, style: &str) -> String {
        format!("{style}{line}{ANSI_RESET}")
    }
}

pub(crate) fn style_for_stdout() -> UsageOutputStyle {
    usage_output_style(
        std::io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
        std::env::var("TERM").ok().as_deref(),
    )
}

fn usage_output_style(stdout_is_tty: bool, no_color: bool, term: Option<&str>) -> UsageOutputStyle {
    if stdout_is_tty && !no_color && term != Some("dumb") {
        UsageOutputStyle::colored()
    } else {
        UsageOutputStyle::plain()
    }
}

fn style_quota_bar(bar: &str, risk_style: Option<&str>) -> String {
    let mut styled = String::new();
    let mut filled = String::new();
    let mut empty = String::new();

    for ch in bar.chars() {
        if ch == QUOTA_BAR_FILLED_CHAR {
            filled.push(ch);
        } else {
            empty.push(ch);
        }
    }

    if let Some(style) = risk_style {
        styled.push_str(&wrap_ansi(&filled, style));
    } else {
        styled.push_str(&filled);
    }
    styled.push_str(&wrap_ansi(&empty, ANSI_DIM));
    styled
}

fn style_usage_left_text(text: &str, risk_style: Option<&str>) -> String {
    match risk_style {
        Some(style) => wrap_ansi(text, style),
        None => text.to_string(),
    }
}

fn wrap_ansi(text: &str, style: &str) -> String {
    if text.is_empty() {
        String::new()
    } else {
        format!("{style}{text}{ANSI_RESET}")
    }
}

fn parse_usage_left_percent(text: &str) -> Option<f64> {
    text.strip_suffix("% left")?.parse().ok()
}

fn usage_left_risk_style(left_percent: f64) -> Option<&'static str> {
    if left_percent <= 0.0 {
        Some(ANSI_BOLD_RED)
    } else if left_percent < 5.0 {
        Some(ANSI_RED)
    } else if left_percent < 20.0 {
        Some(ANSI_YELLOW)
    } else if left_percent >= 50.0 {
        Some(ANSI_GREEN)
    } else {
        None
    }
}

fn is_usage_account_header_line(line: &str) -> bool {
    !line.starts_with(' ')
        && !line.starts_with("additional ")
        && !line.starts_with("plan:")
        && !line.starts_with("status:")
        && !line.starts_with("usage:")
        && !line.starts_with("credits:")
        && !line.starts_with("overall estimate:")
        && !line.contains("┬ quota")
        && !line.contains("└ reset")
        && line.contains('(')
        && line.contains(')')
}

#[cfg(test)]
mod tests {
    use super::{
        ANSI_BOLD, ANSI_BOLD_GREEN, ANSI_BOLD_RED, ANSI_DIM, ANSI_GREEN, ANSI_RED, ANSI_RESET,
        ANSI_YELLOW, ForecastUnavailableReason, UsageOutputStyle, usage_output_style,
    };

    #[test]
    fn usage_output_style_respects_tty_and_no_color() {
        assert_eq!(
            usage_output_style(true, false, Some("xterm-256color")),
            UsageOutputStyle::colored()
        );
        assert_eq!(
            usage_output_style(false, false, Some("xterm-256color")),
            UsageOutputStyle::plain()
        );
        assert_eq!(
            usage_output_style(true, true, Some("xterm-256color")),
            UsageOutputStyle::plain()
        );
        assert_eq!(
            usage_output_style(true, false, Some("dumb")),
            UsageOutputStyle::plain()
        );
    }

    #[test]
    fn plain_usage_output_style_preserves_text() {
        let text = "work (12345678)\nplan: pro\n5-hour ┬ quota [████] 80.0% left\n       └ reset [────] 20% remaining\noverall estimate:\n  unavailable: incomplete usage data";

        assert_eq!(UsageOutputStyle::plain().style_text(text), text);
    }

    #[test]
    fn colored_usage_output_style_applies_usage_hierarchy() {
        let text = "* work (12345678) [UNAVAILABLE]\nplan: pro\nstatus: weekly usage is 100.0%\n5-hour ┬ quota [████░░░░] 80.0% left\n       └ reset [────] 20% remaining\noverall estimate:\n  unavailable: incomplete usage data";

        let styled = UsageOutputStyle::colored().style_text(text);

        assert!(styled.contains(&format!(
            "{ANSI_BOLD_GREEN}*{ANSI_RESET} {ANSI_BOLD}work (12345678){ANSI_RESET} {ANSI_BOLD_RED}[UNAVAILABLE]{ANSI_RESET}"
        )));
        assert!(styled.contains(&format!("{ANSI_DIM}plan: pro{ANSI_RESET}")));
        assert!(styled.contains(&format!(
            "{ANSI_RED}status: weekly usage is 100.0%{ANSI_RESET}"
        )));
        assert!(styled.contains(&format!(
            "\n5-hour ┬ quota [{ANSI_GREEN}████{ANSI_RESET}{ANSI_DIM}░░░░{ANSI_RESET}] {ANSI_GREEN}80.0% left{ANSI_RESET}\n"
        )));
        assert!(styled.contains("\n       └ reset [────] 20% remaining\n"));
        assert!(styled.contains(&format!("{ANSI_BOLD}overall estimate:{ANSI_RESET}")));
        assert!(styled.contains(&format!(
            "{ANSI_DIM}  unavailable: incomplete usage data{ANSI_RESET}"
        )));
    }

    #[test]
    fn colored_usage_output_style_colors_forecast_status() {
        let style = UsageOutputStyle::colored();

        let sustainable = style.style_text(
            "overall estimate:\n  unavailable: not expected within 14d at current pace",
        );
        assert!(sustainable.contains(&format!("{ANSI_BOLD}overall estimate:{ANSI_RESET}")));
        assert!(sustainable.contains(&format!(
            "{ANSI_GREEN}  unavailable: not expected within 14d at current pace{ANSI_RESET}"
        )));

        let unavailable = style.style_text(
            "overall estimate:\n  unavailable: in 4d 19h (Tue 12:54)\n  limited by: weekly",
        );
        assert!(unavailable.contains(&format!(
            "{ANSI_YELLOW}  unavailable: in 4d 19h (Tue 12:54){ANSI_RESET}"
        )));
        assert!(unavailable.contains(&format!("{ANSI_DIM}  limited by: weekly{ANSI_RESET}")));

        for reason in ForecastUnavailableReason::ALL {
            let unavailable = style.style_text(&format!("  unavailable: {}", reason.as_str()));
            assert_eq!(
                unavailable,
                format!("{ANSI_DIM}  unavailable: {}{ANSI_RESET}", reason.as_str())
            );
        }
    }

    #[test]
    fn colored_usage_output_style_colors_quota_by_left_percent() {
        let style = UsageOutputStyle::colored();

        let normal = style.style_text("weekly ┬ quota [████░░░░] 20.0% left");
        assert!(normal.contains(&format!(
            "weekly ┬ quota [████{ANSI_DIM}░░░░{ANSI_RESET}] 20.0% left"
        )));

        let yellow = style.style_text("weekly ┬ quota [██░░░░░░] 10.0% left");
        assert!(yellow.contains(&format!(
            "weekly ┬ quota [{ANSI_YELLOW}██{ANSI_RESET}{ANSI_DIM}░░░░░░{ANSI_RESET}] {ANSI_YELLOW}10.0% left{ANSI_RESET}"
        )));

        let red = style.style_text("weekly ┬ quota [█░░░░░░░] 4.0% left");
        assert!(red.contains(&format!(
            "weekly ┬ quota [{ANSI_RED}█{ANSI_RESET}{ANSI_DIM}░░░░░░░{ANSI_RESET}] {ANSI_RED}4.0% left{ANSI_RESET}"
        )));

        let exhausted = style.style_text("weekly ┬ quota [░░░░░░░░] 0.0% left");
        assert!(exhausted.contains(&format!(
            "weekly ┬ quota [{ANSI_DIM}░░░░░░░░{ANSI_RESET}] {ANSI_BOLD_RED}0.0% left{ANSI_RESET}"
        )));

        let dynamic = style.style_text("monthly ┬ quota [████░░░░] 50.0% left");
        assert!(dynamic.contains(&format!(
            "monthly ┬ quota [{ANSI_GREEN}████{ANSI_RESET}{ANSI_DIM}░░░░{ANSI_RESET}] {ANSI_GREEN}50.0% left{ANSI_RESET}"
        )));
    }

    #[test]
    fn colored_usage_output_style_leaves_credits_plain() {
        let style = UsageOutputStyle::colored();

        assert_eq!(style.style_text("credits: none"), "credits: none");
        assert_eq!(style.style_text("credits: 0"), "credits: 0");
        assert_eq!(style.style_text("credits: 0.5"), "credits: 0.5");
    }

    #[test]
    fn colored_usage_output_style_highlights_expiring_reset_credits() {
        let style = UsageOutputStyle::colored();
        let later = "rate-limit resets: 2 available, next expires in 8d (12:00 on 10 Sep)";
        let soon =
            "rate-limit resets: 2 available, next expires in 7d (12:00 on 9 Sep), expiring soon";

        assert_eq!(style.style_text(later), later);
        assert_eq!(
            style.style_text(soon),
            format!("{ANSI_YELLOW}{soon}{ANSI_RESET}")
        );
    }
}
