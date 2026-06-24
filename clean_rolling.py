import re
import sys

def parse_original_youtube_srt(input_path):
    pattern = r"(\d{2}:\d{2}:\d{2},\d{3})\s*-->\s*(\d{2}:\d{2}:\d{2},\d{3})"
    segments = []
    
    with open(input_path, 'r', encoding='utf-8') as f:
        content = f.read()
        
    lines = content.split('\n')
    i = 0
    while i < len(lines):
        line = lines[i].strip()
        if not line:
            i += 1
            continue
            
        match = re.match(pattern, line)
        if match:
            start_time, end_time = match.groups()
            text_lines = []
            j = i + 1
            while j < len(lines):
                next_line = lines[j].strip()
                if not next_line:
                    j += 1
                    continue
                if re.match(pattern, next_line):
                    break
                if next_line.isdigit():
                    k = j + 1
                    while k < len(lines) and not lines[k].strip():
                        k += 1
                    if k < len(lines) and re.match(pattern, lines[k].strip()):
                        break
                text_lines.append(next_line)
                j += 1
            
            text = " ".join(text_lines).strip()
            if text:
                segments.append({
                    'start': start_time,
                    'end': end_time,
                    'text': text
                })
            i = j
        else:
            i += 1
            
    return segments

def time_to_seconds(t_str):
    h, m, s_ms = t_str.split(':')
    s, ms = s_ms.split(',')
    return int(h)*3600 + int(m)*60 + int(s) + int(ms)/1000.0

def seconds_to_time(secs):
    h = int(secs // 3600)
    m = int((secs % 3600) // 60)
    s = int(secs % 60)
    ms = int(round((secs - int(secs)) * 1000))
    if ms >= 1000:
        s += 1
        ms -= 1000
    return f"{h:02d}:{m:02d}:{s:02d},{ms:03d}"

def clean_youtube_rolling_subtitles(input_path, output_path):
    segs = parse_original_youtube_srt(input_path)
    print(f"Parsed {len(segs)} raw segments.")
    
    linearized = []
    accumulated_words = []

    for seg in segs:
        seg_words = seg['text'].split()
        
        # Find maximum suffix-prefix overlap
        overlap_size = 0
        for k in range(min(len(accumulated_words), len(seg_words)), 0, -1):
            if accumulated_words[-k:] == seg_words[:k]:
                overlap_size = k
                break
                
        new_words = seg_words[overlap_size:]
        if new_words:
            start_sec = time_to_seconds(seg['start'])
            end_sec = time_to_seconds(seg['end'])
            # Since rolling caption blocks can have identical start/end or 10ms diffs:
            if end_sec - start_sec < 0.5:
                end_sec = start_sec + 2.0
                
            linearized.append({
                'start_sec': start_sec,
                'end_sec': end_sec,
                'text': ' '.join(new_words)
            })
            accumulated_words.extend(new_words)

    # Let's increase limits so sentences are longer and sound more natural when spoken.
    # Max words: 24, Max duration: 8.5 seconds.
    final_segments = []
    if not linearized:
        return
        
    curr = linearized[0].copy()
    
    for next_item in linearized[1:]:
        curr_words = curr['text'].split()
        next_words = next_item['text'].split()
        
        gap = next_item['start_sec'] - curr['end_sec']
        comb_words = len(curr_words) + len(next_words)
        comb_dur = next_item['end_sec'] - curr['start_sec']
        
        # Merge if they are close, or if the current segment is too short (< 4.5s)
        if (gap < 2.0 or curr['end_sec'] - curr['start_sec'] < 4.5) and comb_words <= 22 and comb_dur <= 8.0:
            curr['text'] = curr['text'] + " " + next_item['text']
            curr['end_sec'] = max(curr['end_sec'], next_item['end_sec'])
        else:
            final_segments.append(curr)
            curr = next_item.copy()
            
    final_segments.append(curr)

    # Write out as proper SRT
    with open(output_path, 'w', encoding='utf-8') as f:
        for idx, seg in enumerate(final_segments, 1):
            start_str = seconds_to_time(seg['start_sec'])
            end_str = seconds_to_time(seg['end_sec'])
            f.write(f"{idx}\n")
            f.write(f"{start_str} --> {end_str}\n")
            f.write(f"{seg['text']}\n\n")
            
    print(f"Successfully processed {len(segs)} raw -> {len(linearized)} linear -> {len(final_segments)} final sentences.")

if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("Usage: python3 clean_rolling.py INPUT.srt OUTPUT.srt")
    clean_youtube_rolling_subtitles(sys.argv[1], sys.argv[2])
