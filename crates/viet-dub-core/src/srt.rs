use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use regex::Regex;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SrtSegment {
    pub index: usize,
    pub start: String,
    pub end: String,
    pub text: String,
}

pub fn parse_srt(path: &Path) -> Result<Vec<SrtSegment>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read SRT {}", path.display()))?;
    parse_srt_content(&content)
}

pub fn time_to_seconds(time: &str) -> Result<f64> {
    let parts: Vec<_> = time.split(':').collect();
    if parts.len() != 3 {
        bail!("Invalid SRT timestamp: {time}");
    }
    let seconds: Vec<_> = parts[2].split(',').collect();
    if seconds.len() != 2 {
        bail!("Invalid SRT timestamp: {time}");
    }

    let hours: i64 = parts[0]
        .trim()
        .parse()
        .with_context(|| format!("Invalid SRT timestamp: {time}"))?;
    let minutes: i64 = parts[1]
        .trim()
        .parse()
        .with_context(|| format!("Invalid SRT timestamp: {time}"))?;
    let whole_seconds: i64 = seconds[0]
        .trim()
        .parse()
        .with_context(|| format!("Invalid SRT timestamp: {time}"))?;
    let milliseconds: i64 = seconds[1]
        .trim()
        .parse()
        .with_context(|| format!("Invalid SRT timestamp: {time}"))?;

    Ok((hours * 3600 + minutes * 60 + whole_seconds) as f64 + milliseconds as f64 / 1000.0)
}

fn parse_srt_content(content: &str) -> Result<Vec<SrtSegment>> {
    let content = content.replace("\r\n", "\n").replace('\r', "\n");
    let timing = Regex::new(r"^(\d{2}:\d{2}:\d{2},\d{3}) --> (\d{2}:\d{2}:\d{2},\d{3})")?;
    let mut segments = Vec::new();

    for block in content.trim().split("\n\n") {
        let lines: Vec<_> = block
            .split('\n')
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        if lines.len() < 3 {
            continue;
        }

        let Some(captures) = timing.captures(lines[1]) else {
            continue;
        };
        segments.push(SrtSegment {
            index: lines[0]
                .parse()
                .with_context(|| format!("Invalid SRT index: {}", lines[0]))?,
            start: captures[1].to_owned(),
            end: captures[2].to_owned(),
            text: lines[2..].join(" "),
        });
    }

    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_blocks_like_python() {
        let input = concat!(
            "1\r\n",
            "00:00:01,250 --> 00:00:03,500 align:start\r\n",
            " first line \r\n",
            "second line\r\n\r\n",
            "2\r\n",
            "00:00:04,000  -->  00:00:05,000\r\n",
            "strict spacing skips this block\r\n",
        );

        assert_eq!(
            parse_srt_content(input).unwrap(),
            vec![SrtSegment {
                index: 1,
                start: "00:00:01,250".to_owned(),
                end: "00:00:03,500".to_owned(),
                text: "first line second line".to_owned(),
            }]
        );
        assert!((time_to_seconds("01:02:03,004").unwrap() - 3723.004).abs() < f64::EPSILON);
    }
}
