use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use regex::Regex;

use crate::srt::time_to_seconds;

#[derive(Clone, Debug, PartialEq)]
struct RawSegment {
    start: String,
    end: String,
    text: String,
}

#[derive(Clone, Debug, PartialEq)]
struct TimedText {
    start_sec: f64,
    end_sec: f64,
    text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CleanStats {
    pub raw: usize,
    pub linear: usize,
    pub final_segments: usize,
}

pub fn clean_youtube_rolling_subtitles(input: &Path, output: &Path) -> Result<CleanStats> {
    let content = fs::read_to_string(input)
        .with_context(|| format!("Failed to read SRT {}", input.display()))?;
    let raw = parse_original_youtube_srt(&content)?;
    let linearized = linearize(&raw)?;
    let final_segments = merge_sentences(&linearized);
    let stats = CleanStats {
        raw: raw.len(),
        linear: linearized.len(),
        final_segments: final_segments.len(),
    };

    // Python returns before opening the output when no segment survives.
    if linearized.is_empty() {
        return Ok(stats);
    }

    fs::write(output, format_srt(&final_segments))
        .with_context(|| format!("Failed to write SRT {}", output.display()))?;
    Ok(stats)
}

pub fn seconds_to_time(seconds: f64) -> String {
    let hours = (seconds / 3600.0).floor() as i64;
    let minutes = (seconds.rem_euclid(3600.0) / 60.0).floor() as i64;
    let mut whole_seconds = seconds.rem_euclid(60.0) as i64;
    let mut milliseconds = ((seconds - seconds.trunc()) * 1000.0).round_ties_even() as i64;
    if milliseconds >= 1000 {
        whole_seconds += 1;
        milliseconds -= 1000;
    }
    format!("{hours:02}:{minutes:02}:{whole_seconds:02},{milliseconds:03}")
}

fn parse_original_youtube_srt(content: &str) -> Result<Vec<RawSegment>> {
    let content = content.replace("\r\n", "\n").replace('\r', "\n");
    let timing = Regex::new(r"^(\d{2}:\d{2}:\d{2},\d{3})\s*-->\s*(\d{2}:\d{2}:\d{2},\d{3})")?;
    let lines: Vec<_> = content.split('\n').collect();
    let mut segments = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();
        if line.is_empty() {
            i += 1;
            continue;
        }

        let Some(captures) = timing.captures(line) else {
            i += 1;
            continue;
        };
        let start = captures[1].to_owned();
        let end = captures[2].to_owned();
        let mut text_lines = Vec::new();
        let mut j = i + 1;

        while j < lines.len() {
            let next_line = lines[j].trim();
            if next_line.is_empty() {
                j += 1;
                continue;
            }
            if timing.is_match(next_line) {
                break;
            }
            if next_line.chars().all(char::is_numeric) {
                let mut k = j + 1;
                while k < lines.len() && lines[k].trim().is_empty() {
                    k += 1;
                }
                if k < lines.len() && timing.is_match(lines[k].trim()) {
                    break;
                }
            }
            text_lines.push(next_line);
            j += 1;
        }

        let text = text_lines.join(" ").trim().to_owned();
        if !text.is_empty() {
            segments.push(RawSegment { start, end, text });
        }
        i = j;
    }

    Ok(segments)
}

fn linearize(segments: &[RawSegment]) -> Result<Vec<TimedText>> {
    let mut accumulated_words: Vec<String> = Vec::new();
    let mut linearized = Vec::new();

    for segment in segments {
        let words: Vec<_> = segment.text.split_whitespace().map(str::to_owned).collect();
        let overlap = (1..=accumulated_words.len().min(words.len()))
            .rev()
            .find(|&size| accumulated_words[accumulated_words.len() - size..] == words[..size])
            .unwrap_or(0);
        let new_words = &words[overlap..];
        if new_words.is_empty() {
            continue;
        }

        let start_sec = time_to_seconds(&segment.start)?;
        let mut end_sec = time_to_seconds(&segment.end)?;
        if end_sec - start_sec < 0.5 {
            end_sec = start_sec + 2.0;
        }
        linearized.push(TimedText {
            start_sec,
            end_sec,
            text: new_words.join(" "),
        });
        accumulated_words.extend_from_slice(new_words);
    }

    Ok(linearized)
}

fn merge_sentences(linearized: &[TimedText]) -> Vec<TimedText> {
    let Some(first) = linearized.first() else {
        return Vec::new();
    };
    let mut merged = Vec::new();
    let mut current = first.clone();

    for next in &linearized[1..] {
        if can_merge(&current, next) {
            current.text.push(' ');
            current.text.push_str(&next.text);
            current.end_sec = current.end_sec.max(next.end_sec);
        } else {
            merged.push(current);
            current = next.clone();
        }
    }
    merged.push(current);
    merged
}

fn can_merge(current: &TimedText, next: &TimedText) -> bool {
    let gap = next.start_sec - current.end_sec;
    let combined_words =
        current.text.split_whitespace().count() + next.text.split_whitespace().count();
    let combined_duration = next.end_sec - current.start_sec;

    if gap > 2.0 || combined_words > 26 || combined_duration > 8.5 {
        return false;
    }

    !ends_sentence(&current.text) || current.end_sec - current.start_sec < 2.0
}

fn ends_sentence(text: &str) -> bool {
    let text = text
        .trim()
        .trim_end_matches(&['"', '\'', '”', '’', ')', ']', '}'][..]);
    matches!(text.chars().last(), Some('.' | '!' | '?' | '…'))
}

fn format_srt(segments: &[TimedText]) -> String {
    let mut output = String::new();
    for (index, segment) in segments.iter().enumerate() {
        writeln!(output, "{}", index + 1).unwrap();
        writeln!(
            output,
            "{} --> {}",
            seconds_to_time(segment.start_sec),
            seconds_to_time(segment.end_sec)
        )
        .unwrap();
        writeln!(output, "{}\n", segment.text).unwrap();
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linearizes_overlap_and_formats_segments() {
        let input = concat!(
            "ignored header\n",
            "1\n00:00:00,000  -->   00:00:00,400 trailing data\nHello\nworld\n\n",
            "2\n00:00:00,400 --> 00:00:01,000\nHello world again\n\n",
            "3\n00:00:01,000 --> 00:00:01,200\nHello world again\n\n",
            "4\n00:00:04,100 --> 00:00:05,000\ntail\n\n",
            "5\n00:00:10,000 --> 00:00:11,000\nstop\n",
        );

        let raw = parse_original_youtube_srt(input).unwrap();
        let linearized = linearize(&raw).unwrap();
        let final_segments = merge_sentences(&linearized);

        assert_eq!(
            (raw.len(), linearized.len(), final_segments.len()),
            (5, 4, 3)
        );
        assert_eq!(
            format_srt(&final_segments),
            concat!(
                "1\n00:00:00,000 --> 00:00:02,000\nHello world again\n\n",
                "2\n00:00:04,100 --> 00:00:05,000\ntail\n\n",
                "3\n00:00:10,000 --> 00:00:11,000\nstop\n\n",
            )
        );
        assert_eq!(seconds_to_time(59.9996), "00:00:60,000");
    }

    #[test]
    fn merge_keeps_complete_sentences_but_joins_fragments() {
        let complete = TimedText {
            start_sec: 0.0,
            end_sec: 3.0,
            text: "The application is ready.".to_owned(),
        };
        let fragment = TimedText {
            start_sec: 0.0,
            end_sec: 3.0,
            text: "The application is".to_owned(),
        };
        let next = TimedText {
            start_sec: 3.1,
            end_sec: 5.0,
            text: "ready to run.".to_owned(),
        };

        assert!(!can_merge(&complete, &next));
        assert!(can_merge(&fragment, &next));
    }
}
