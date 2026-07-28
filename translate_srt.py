import sys
import re
import time
import json
import os
from pathlib import Path

import requests


GEMINI_MODEL = "gemini-3.5-flash-lite"
GEMINI_ENDPOINT = "https://generativelanguage.googleapis.com/v1beta/interactions"

SOURCE_CORRECTIONS = (
    (r"\bcompare it vector\b", "Comparator"),
    (r"\bcomposite\b", "Comparator"),
    (r"\bCompact\b", "Comparator"),
    (r"\bcompared to\b", "compareTo"),
    (r"\bcomparative method\b", "compareTo method"),
    (r"\bcompatible\b", "Comparable"),
    (r"\bcomparative\b", "Comparable"),
    (r"\bcollection class\b", "Collections class"),
    (r"\bclass called collections\b", "class called Collections"),
    (r"\bnew at least\b", "new ArrayList"),
    (r"\barray or address\b", "ArrayList"),
    (r"\btwo streamers\b", "toString"),
    (r"\b(?:a )?two string method(?: name)?\b", "a toString method"),
    (r"\bsource action\b", "toString"),
    (r"\btitle operator\b", "ternary operator"),
    (r"\bturn it operator\b", "ternary operator"),
    (r"\bvapor classes\b", "wrapper classes"),
    (r"\bglass names\b", "class names"),
    (r"\bdot h\b", ".age"),
    (r"\bcompile itself\b", "compare itself"),
    (r"\bremove a Lambda expression\b", "use a Lambda expression"),
)

TECH_TERMS = (
    (r"\bfunctional interfaces?\b", "functional interface"),
    (r"\banonymous interfaces?\b", "anonymous class"),
    (r"\banonymous classes?\b", "anonymous class"),
    (r"\bwrapper classes?\b", "wrapper class"),
    (r"\bternary operators?\b", "ternary operator"),
    (r"\blambda expressions?\b", "Lambda expression"),
    (r"\bcompareTo\b", "compareTo"),
    (r"\btoString\b", "toString"),
    (r"\bComparators?\b", "Comparator"),
    (r"\bComparables?\b", "Comparable"),
    (r"\bCollections\b", "Collections"),
    (r"\bCollection\b", "Collection"),
    (r"\bArrayList\b", "ArrayList"),
    (r"\bLists?\b", "List"),
    (r"\bIntegers?\b", "Integer"),
    (r"\bStrings?\b", "String"),
    (r"\bStudents?\b", "Student"),
    (r"\binterfaces?\b", "interface"),
    (r"\bclasses\b", "classes"),
    (r"\bclass\b", "class"),
    (r"\bmethods?\b", "method"),
    (r"\bobjects?\b", "object"),
    (r"\bsorting\b", "sorting"),
    (r"\bsort\b", "sort"),
    (r"\bcompare\b", "compare"),
    (r"\bswap\b", "swap"),
    (r"\breturn\b", "return"),
    (r"\bpublic\b", "public"),
    (r"\bint\b", "int"),
    (r"\bnew\b", "new"),
    (r"\bsyntax\b", "syntax"),
    (r"\bpackage\b", "package"),
    (r"\bUML\b", "UML"),
    (r"\bUnified Modeling Language\b", "Unified Modeling Language"),
    (r"\bCommunication Diagrams?\b", "Communication Diagram"),
    (r"\bComponent Diagrams?\b", "Component Diagram"),
    (r"\bDeployment Diagrams?\b", "Deployment Diagram"),
    (r"\bSequence Diagrams?\b", "Sequence Diagram"),
    (r"\bState Machine Diagrams?\b", "State Machine Diagram"),
    (r"\bTiming Diagrams?\b", "Timing Diagram"),
    (r"\bActivity Diagrams?\b", "Activity Diagram"),
    (r"\bClass Diagrams?\b", "Class Diagram"),
    (r"\bActors?\b", "Actor"),
    (r"\bUse Cases?\b", "Use Case"),
    (r"\bLifelines?\b", "Lifeline"),
    (r"\bMessages?\b", "Message"),
    (r"\bNodes?\b", "Node"),
    (r"\bStates?\b", "State"),
    (r"\bTransitions?\b", "Transition"),
    (r"\bStereotypes?\b", "Stereotype"),
)

def normalize_tech_terms(text):
    for pattern, replacement in SOURCE_CORRECTIONS:
        text = re.sub(pattern, replacement, text, flags=re.IGNORECASE)
    return text

def protect_tech_terms(text):
    for index, (pattern, _) in enumerate(TECH_TERMS):
        text = re.sub(
            pattern,
            f"ZXQTERM{index:03d}QXZ",
            text,
            flags=re.IGNORECASE,
        )
    return text

def restore_tech_terms(text):
    for index, (_, term) in enumerate(TECH_TERMS):
        text = re.sub(
            f"ZXQTERM{index:03d}QXZ",
            term,
            text,
            flags=re.IGNORECASE,
        )
    return text

def write_srt(path, segments, text_key):
    with open(path, "w", encoding="utf-8") as f:
        for idx, segment in enumerate(segments, 1):
            f.write(f"{idx}\n")
            f.write(f"{segment['start']} --> {segment['end']}\n")
            f.write(f"{segment[text_key]}\n\n")

def parse_srt(path):
    with open(path, 'r', encoding='utf-8') as f:
        content = f.read()
    
    blocks = content.strip().split('\n\n')
    segments = []
    
    for block in blocks:
        lines = [l.strip() for l in block.split('\n') if l.strip()]
        if len(lines) < 3:
            continue
            
        index = lines[0]
        time_range = lines[1]
        text = " ".join(lines[2:]).strip()
        
        match = re.match(r"(\d{2}:\d{2}:\d{2},\d{3}) --> (\d{2}:\d{2}:\d{2},\d{3})", time_range)
        if not match:
            continue
        start_time, end_time = match.groups()
        
        segments.append({
            'index': index,
            'start': start_time,
            'end': end_time,
            'text': text
        })
    return segments

def gemini_api_keys(values=None):
    if not os.environ.get("GEMINI_API_KEYS"):
        env_path = Path(__file__).resolve().parent / ".env"
        if env_path.is_file():
            for line in env_path.read_text(encoding="utf-8").splitlines():
                if line.startswith("GEMINI_API_KEYS="):
                    os.environ["GEMINI_API_KEYS"] = line.split("=", 1)[1].strip().strip("\"'")
                    break
    values = values or [os.environ.get("GEMINI_API_KEYS", "")]
    return [key.strip() for value in values for key in re.split(r"[,\n]", value) if key.strip()]


def translate_batch(batch, source_language, api_key):
    source = json.dumps(
        [{"index": segment["index"], "text": protect_tech_terms(segment["text"])} for segment in batch],
        ensure_ascii=False,
    )
    prompt = (
        "Translate these subtitle segments from "
        f"{source_language} to Vietnamese. Return one concise, natural Vietnamese text for every "
        "input index. Preserve technical placeholders beginning ZXQTERM and never add commentary. "
        "The timestamps are managed by the caller; remove repeated caption wording and make sentence "
        f"breaks read naturally. Input JSON: {source}"
    )
    schema = {
        "type": "object",
        "properties": {
            "segments": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {"index": {"type": "integer"}, "text": {"type": "string"}},
                    "required": ["index", "text"],
                },
            }
        },
        "required": ["segments"],
    }
    response = requests.post(
        GEMINI_ENDPOINT,
        headers={"x-goog-api-key": api_key, "Content-Type": "application/json"},
        json={
            "model": GEMINI_MODEL,
            "input": prompt,
            "store": False,
            "response_format": {"type": "text", "mime_type": "application/json", "schema": schema},
        },
        timeout=90,
    )
    response.raise_for_status()
    payload = response.json()
    texts = [
        content["text"]
        for step in payload.get("steps", [])
        if step.get("type") == "model_output"
        for content in step.get("content", [])
        if content.get("type") == "text"
    ]
    if not texts:
        raise RuntimeError("Gemini returned no text output")
    translated = json.loads("".join(texts))["segments"]
    by_index = {item.get("index"): item.get("text", "").strip() for item in translated}
    expected = [segment["index"] for segment in batch]
    if set(by_index) != set(expected) or any(not by_index[index] for index in expected):
        raise RuntimeError("Gemini returned incomplete subtitle segments")
    return [restore_tech_terms(by_index[segment["index"]]) for segment in batch]


def translate_srt(input_srt, output_srt, normalized_srt=None, source_language="en", api_keys=None):
    print("Parsing final clean SRT...")
    segs = parse_srt(input_srt)
    print(f"Loaded {len(segs)} segments for translation.")

    if source_language.lower().startswith("en"):
        for segment in segs:
            segment['text'] = normalize_tech_terms(segment['text'])
    if normalized_srt:
        write_srt(normalized_srt, segs, "text")
        print(f"Saved normalized source to {normalized_srt}")
    
    keys = gemini_api_keys(api_keys)
    if not keys:
        raise RuntimeError("Missing GEMINI_API_KEYS. Add one or more Gemini API keys to .env.")

    translated_segs = []
    batch_size = 200
    for i in range(0, len(segs), batch_size):
        batch = segs[i:i+batch_size]
        try:
            translated_texts = translate_batch(batch, source_language, keys[(i // batch_size) % len(keys)])
            for idx, s in enumerate(batch):
                s['translated_text'] = translated_texts[idx]
                translated_segs.append(s)
            print(f"Translated batch {i // batch_size + 1}/{((len(segs) - 1) // batch_size) + 1}")
        except Exception as e:
            raise RuntimeError(f"Gemini translation failed for batch {i // batch_size + 1}: {e}") from e
                
    # Save the output
    write_srt(output_srt, translated_segs, "translated_text")
            
    print(f"Saved {output_srt}")

if __name__ == "__main__":
    if len(sys.argv) not in {3, 4, 5}:
        raise SystemExit("Usage: python3 translate_srt.py INPUT.srt OUTPUT_VI.srt [NORMALIZED_SOURCE.srt] [SOURCE_LANGUAGE]")
    normalized = sys.argv[3] if len(sys.argv) >= 4 else None
    source_language = sys.argv[4] if len(sys.argv) == 5 else "en"
    translate_srt(sys.argv[1], sys.argv[2], normalized, source_language)
