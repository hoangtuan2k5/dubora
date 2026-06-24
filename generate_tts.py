import re
import os
import sys
import asyncio
import subprocess
import requests
import edge_tts
from gtts import gTTS

AZURE_VOICE = "de-DE-Seraphina:DragonHDLatestNeural"
AZURE_REGION = "southeastasia"
AZURE_ENV_PATHS = (
    "/home/ubuntu/AI_news/.env",
    "/home/ubuntu/viet-dub/.env",
)

TTS_PRONUNCIATIONS = (
    (r"\bRESTful\b", "rét phồ"),
    (r"\bREST\b", "rét"),
    (r"\bCRUD\b", "cờ rút"),
    (r"\bAPI\b", "ây pi ai"),
    (r"\bJSON\b", "giây sần"),
    (r"\bSQL\b", "ét quy eo"),
    (r"\bMySQL\b", "mai ét quy eo"),
    (r"\bJPA\b", "jay pi ây"),
    (r"\bJDBC\b", "jay đi bi xi"),
    (r"\bMVC\b", "em vi xi"),
    (r"\bAOP\b", "ây ô pi"),
    (r"\bHTTP\b", "hát ti ti pi"),
    (r"\bHTTPS\b", "hát ti ti pi ét"),
    (r"\bURL\b", "iu a eo"),
)


def prepare_tts_text(text):
    for pattern, replacement in TTS_PRONUNCIATIONS:
        text = re.sub(pattern, replacement, text)
    return text

def load_env_file(path):
    if not os.path.exists(path):
        return

    with open(path, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            key, value = line.split("=", 1)
            key = key.strip()
            value = value.strip().strip('"').strip("'")
            os.environ.setdefault(key, value)

def load_azure_config():
    for env_path in AZURE_ENV_PATHS:
        load_env_file(env_path)

    api_key = os.environ.get("AZURE_KEY")
    region = os.environ.get("AZURE_REGION", AZURE_REGION)
    if not api_key:
        raise RuntimeError(
            "Missing AZURE_KEY. Add it to /home/ubuntu/AI_news/.env or export it."
        )
    return api_key, region

def escape_xml(text):
    return (
        text.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
        .replace("'", "&apos;")
    )

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
            'index': int(index),
            'start': start_time,
            'end': end_time,
            'text': text
        })
    return segments

def is_valid_audio_file(path):
    if not os.path.exists(path) or os.path.getsize(path) <= 0:
        return False

    try:
        result = subprocess.run(
            [
                "ffprobe", "-v", "error",
                "-show_entries", "format=duration",
                "-of", "default=noprint_wrappers=1:nokey=1",
                path,
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        return float(result.stdout.strip()) > 0.05
    except Exception:
        return False

async def generate_speech_for_segment(index, text, output_path, voice="vi-VN-NamMinhNeural", retries=3):
    if not text.strip():
        return False

    text = prepare_tts_text(text)
    tmp_path = f"{output_path}.tmp"

    for attempt in range(retries):
        try:
            if os.path.exists(tmp_path):
                os.remove(tmp_path)
            communicate = edge_tts.Communicate(text, voice, rate="+8%")
            await asyncio.wait_for(communicate.save(tmp_path), timeout=20.0)
            if is_valid_audio_file(tmp_path):
                os.replace(tmp_path, output_path)
                return True
        except Exception as e:
            print(f"Error at segment {index} (attempt {attempt+1}/{retries}): {type(e).__name__}: {e}")
            await asyncio.sleep(1.0)
        finally:
            if os.path.exists(tmp_path):
                os.remove(tmp_path)

    return False

async def generate_speech_with_gtts(index, text, output_path, retries=3):
    if not text.strip():
        return False

    text = prepare_tts_text(text)
    tmp_path = f"{output_path}.tmp"

    for attempt in range(retries):
        try:
            if os.path.exists(tmp_path):
                os.remove(tmp_path)
            speech = gTTS(text=text, lang="vi", slow=False)
            await asyncio.wait_for(
                asyncio.to_thread(speech.save, tmp_path),
                timeout=20.0,
            )
            if is_valid_audio_file(tmp_path):
                os.replace(tmp_path, output_path)
                return True
        except Exception as e:
            print(f"gTTS error at segment {index} (attempt {attempt+1}/{retries}): {e}")
            await asyncio.sleep(1.0)
        finally:
            if os.path.exists(tmp_path):
                os.remove(tmp_path)

    return False

async def generate_speech_with_azure(index, text, output_path, retries=3):
    if not text.strip():
        return False

    api_key, region = load_azure_config()
    tmp_path = f"{output_path}.tmp"
    url = f"https://{region}.tts.speech.microsoft.com/cognitiveservices/v1"
    headers = {
        "Ocp-Apim-Subscription-Key": api_key,
        "Content-Type": "application/ssml+xml",
        "X-Microsoft-OutputFormat": "audio-48khz-192kbitrate-mono-mp3",
        "User-Agent": "viet-dub",
    }
    ssml = (
        '<speak version="1.0" '
        'xmlns="http://www.w3.org/2001/10/synthesis" '
        'xmlns:mstts="http://www.w3.org/2001/mstts" '
        'xml:lang="vi-VN">'
        f'<voice name="{AZURE_VOICE}">'
        '<prosody rate="0%">'
        f"{escape_xml(text)}"
        "</prosody>"
        "</voice>"
        "</speak>"
    )

    for attempt in range(retries):
        try:
            if os.path.exists(tmp_path):
                os.remove(tmp_path)
            response = await asyncio.wait_for(
                asyncio.to_thread(
                    requests.post,
                    url,
                    headers=headers,
                    data=ssml.encode("utf-8"),
                    timeout=60,
                ),
                timeout=70,
            )
            if response.status_code != 200:
                print(
                    f"Azure error at segment {index} "
                    f"(attempt {attempt+1}/{retries}): "
                    f"{response.status_code} {response.text[:300]}"
                )
                await asyncio.sleep(1.0)
                continue

            with open(tmp_path, "wb") as f:
                f.write(response.content)
            if is_valid_audio_file(tmp_path):
                os.replace(tmp_path, output_path)
                return True
        except Exception as e:
            print(f"Azure error at segment {index} (attempt {attempt+1}/{retries}): {type(e).__name__}: {e}")
            await asyncio.sleep(1.0)
        finally:
            if os.path.exists(tmp_path):
                os.remove(tmp_path)

    return False

async def generate_all_tts(provider="edge", input_srt="/tmp/dubbing/clean_final_vi.srt", output_dir="/tmp/dubbing/tts"):
    segs = parse_srt(input_srt)
    print(f"Loaded {len(segs)} segments for TTS generation.")
    
    os.makedirs(output_dir, exist_ok=True)
    
    missing = [
        s for s in segs
        if not is_valid_audio_file(os.path.join(output_dir, f"seg_{s['index']}.mp3"))
    ]
    print(f"Need to generate {len(missing)} missing or invalid segments.")

    if not missing:
        return

    concurrency = 4 if provider in {"edge", "azure"} else 2
    semaphore = asyncio.Semaphore(concurrency)
    failed = []

    async def worker(s):
        out_file = os.path.join(output_dir, f"seg_{s['index']}.mp3")
        async with semaphore:
            await asyncio.sleep(0.2)
            if is_valid_audio_file(out_file):
                return s['index'], True

            if provider == "azure":
                success = await generate_speech_with_azure(
                    s['index'], s['text'], out_file
                )
            elif provider == "gtts":
                success = await generate_speech_with_gtts(
                    s['index'], s['text'], out_file
                )
            else:
                success = await generate_speech_for_segment(
                    s['index'], s['text'], out_file
                )
            return s['index'], success

    tasks = [asyncio.create_task(worker(s)) for s in missing]

    for completed, task in enumerate(asyncio.as_completed(tasks), 1):
        segment_index, success = await task
        if provider == "azure":
            provider_label = "Azure Seraphina"
        elif provider == "gtts":
            provider_label = "gTTS"
        else:
            provider_label = "Edge TTS"

        if success:
            print(f"Generated {completed}/{len(missing)} with {provider_label}: segment {segment_index}")
        else:
            print(f"FAILED to generate segment {segment_index}")
            failed.append(segment_index)

    if failed:
        raise RuntimeError(f"Failed to generate {len(failed)} segments: {failed}")

if __name__ == "__main__":
    provider = sys.argv[1] if len(sys.argv) > 1 else "azure"
    if provider not in {"azure", "edge", "gtts"}:
        raise SystemExit("Usage: python3 generate_tts.py [azure|edge|gtts] [INPUT_VI.srt] [OUTPUT_TTS_DIR]")
    input_srt = sys.argv[2] if len(sys.argv) > 2 else "/tmp/dubbing/clean_final_vi.srt"
    output_dir = sys.argv[3] if len(sys.argv) > 3 else "/tmp/dubbing/tts"
    asyncio.run(generate_all_tts(provider, input_srt, output_dir))
