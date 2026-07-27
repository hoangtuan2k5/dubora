import sys
import re
import time
from deep_translator import GoogleTranslator

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

def translate_srt(input_srt, output_srt, normalized_srt=None):
    print("Parsing final clean SRT...")
    segs = parse_srt(input_srt)
    print(f"Loaded {len(segs)} segments for translation.")

    for segment in segs:
        segment['text'] = normalize_tech_terms(segment['text'])
    if normalized_srt:
        write_srt(normalized_srt, segs, "text")
        print(f"Saved normalized source to {normalized_srt}")
    
    translator = GoogleTranslator(source='en', target='vi')
    
    # We translate in batches or one-by-one. Since we have 176 segments, we can translate them in batches
    # of 20 to speed it up and avoid rate limiting.
    translated_segs = []
    
    batch_size = 20
    for i in range(0, len(segs), batch_size):
        batch = segs[i:i+batch_size]
        texts = [protect_tech_terms(s['text']) for s in batch]
        
        try:
            # We can join with a unique separator like " ||| " so we can translate them in one call
            combined_text = " ||| ".join(texts)
            translated_combined = translator.translate(combined_text)
            
            # Split back
            translated_texts = [t.strip() for t in translated_combined.split("|||")]
            
            # Check length matching
            if len(translated_texts) != len(texts):
                print(f"Warning: batch size mismatch (expected {len(texts)}, got {len(translated_texts)}). Translating individually...")
                # Fallback to individual
                translated_texts = []
                for s in batch:
                    translated_texts.append(
                        translator.translate(protect_tech_terms(s['text']))
                    )
                    time.sleep(0.2)
            
            for idx, s in enumerate(batch):
                translated_text = restore_tech_terms(translated_texts[idx])
                
                s['translated_text'] = translated_text
                translated_segs.append(s)
                
            print(f"Translated batch {i // batch_size + 1}/{((len(segs) - 1) // batch_size) + 1}")
            time.sleep(0.5)
            
        except Exception as e:
            print(f"Error in batch: {e}. Translating individually...")
            for s in batch:
                try:
                    protected_text = protect_tech_terms(s['text'])
                    s['translated_text'] = restore_tech_terms(
                        translator.translate(protected_text)
                    )
                except Exception as e2:
                    print(f"Individual error for '{s['text']}': {e2}")
                    s['translated_text'] = s['text'] # fallback to English
                translated_segs.append(s)
                time.sleep(0.2)
                
    # Save the output
    write_srt(output_srt, translated_segs, "translated_text")
            
    print(f"Saved {output_srt}")

if __name__ == "__main__":
    if len(sys.argv) not in {3, 4}:
        raise SystemExit("Usage: python3 translate_srt.py INPUT.srt OUTPUT_VI.srt [NORMALIZED_SOURCE.srt]")
    normalized = sys.argv[3] if len(sys.argv) == 4 else None
    translate_srt(sys.argv[1], sys.argv[2], normalized)
