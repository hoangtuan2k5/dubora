# Changelog

All notable changes to Dubora are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The Windows release workflow reads the **latest numbered section** below for the
GitHub Release tag (`vMAJOR.MINOR.PATCH`) and release notes. Add a new version
heading when you want a new GitHub Release; `## [Unreleased]` is ignored.

## [Unreleased]

### Fixed

- Spell dotted model versions such as Qwen 3.8 as `ba chấm tám` instead of thirty-eight
- Spell GPU SKUs such as RTX 4060 digit-by-digit (`bốn không sáu không`)
- Natural Voice companion `.vi-dub.srt` keeps whole sentences timed to speech, while display text preserves IPs, URLs, paths, and model numbers
- Pack unused same-island slack through fitted utterances so a later over-cap cue can stay within 1.13x

## [0.2.0] - 2026-08-26

Natural Voice production cut: keep speech at or below 1.13x, publish duration-safe
jobs instead of fail-closing on unsatisfiable anchors, and ship the three
recommended Azure voices.

### Added

- Natural Voice speech plans with Gemini, source-aligned sync islands, and
  Dragon HD continuous (unanchored) utterances
- Recommended voices in the desktop picker and CLI help: Florian HD,
  `vi-VN-NamMinhNeural`, `vi-VN-HoaiMyNeural`
- Capability-aware delivery SSML: clause breaks, neural action prosody, HD
  `enhancePronunciation`
- Landing-pace variation on slack slots (neural rate contour; HD breaks only)
- Opt-in voiceover polish: ducked original bed, loudnorm toward -17 LUFS,
  80 Hz high-pass
- Pause-fit encoder tail recovery and mixer fades that do not cut speech
- Measured spillback into unused preceding same-island tails
- On-screen IP / URL / path deixis so spoken text can stay inside the tempo cap
  while display SRT stays unchanged

### Changed

- Time-stretch ceiling is 1.13x; P95 1.08 remains a quality target, not a gate
- Speech-plan cache is `gemini-speech-plan-v25`
- Azure SSML duration repair may use up to 20% rate before a concise rewrite

### Fixed

- Dragon HD no longer fail-closes the job when semantic anchors cannot be timed
- Incomplete Gemini concise-revision batches retry missing parts
- Stolen `nhấn Enter` tails are restored so the action stays inside the slot
- Duration-safe early tail-action drift no longer fail-closes an otherwise
  aligned Caddy-style render
- False hard anchors from commentary (`look pretty good`, `Open Router`,
  `I can open`)
- Invalid planner pauses drop instead of aborting the whole document
- One-millisecond over-slot ceil no longer trips reviewRequired

## [0.1.0] - 2026-07-11

First Dubora cut: shared Rust pipeline, Linux CLI, and Windows desktop app.

### Added

- `dubora-core` pipeline: clean rolling captions, Azure TTS, FFmpeg rubberband
  mix, video copy + AAC `vie` mux
- `dubora` Linux CLI with `--force`, `--force-tts`, `--translate`, and
  directory targets
- Windows Tauri 2 app with library, branded NSIS installer, and usage guide
- Azure voice list, styles, and IPv4/IPv6 spoken forms
- Gemini English-to-Vietnamese subtitle translation
- YouTube import with subtitle fallback and quality selection
- Companion `.vi-dub.srt` next to the dubbed MP4
- Video hub with embedded playback

### Fixed

- Stale TTS cache invalidation
- FFmpeg console windows and NVIDIA dGPU routing on Windows
- WebView2 black window during dub
- YouTube imports kept as MP4 with repaired subtitles

[Unreleased]: https://github.com/hoangtuan2k5/dubora/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/hoangtuan2k5/dubora/releases/tag/v0.2.0
[0.1.0]: https://github.com/hoangtuan2k5/dubora/releases/tag/v0.1.0
