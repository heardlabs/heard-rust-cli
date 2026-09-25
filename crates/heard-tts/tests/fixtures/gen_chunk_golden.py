"""Regenerate chunk_golden.json from the REAL kokoro-onnx chunker.

Dev-only: needs kokoro-onnx 0.6.1 importable (the version the Heard app
ships). Run with `python3 -B gen_chunk_golden.py` from this directory; it
writes chunk_golden.json next to itself and touches nothing else.
"""

import json
import pathlib

from kokoro_onnx.chunker import pause_after, split_phonemes

S1 = "ðə bˈɪld ɪz ɡɹˈin ænd ðə tˈɛsts ˈɔl pˈæs."
S2 = "hˈɜɹd spˈiks jʊɹ ˈAʤᵊnts wˈɜɹk ˈWt lˈWd, sˌO ju kæn lˈʊk əwˈA."
S3 = "kəkˈɔɹO ɹˈʌnz lˈOkəli; nˈʌθɪŋ lˈivz ðɪs məʃˈin."
S4 = "ˈI ɹifˈæktəɹd ðə ʤˈAsᵊn kˈɑnfəɡ ænd ˈOpᵊnd ɐ pˌiˈɑɹ ˌɔn ɡˈɪthˌʌb!"
LONG_WORD = "ʃ" * 1200

CASES = [
    ("empty", "", 510),
    ("blank", "  \n ", 510),
    ("one_sentence", S1, 510),
    ("four_sentences", " ".join([S1, S2, S3, S4]), 510),
    ("twelve_sentences_510", " ".join([S1, S2, S3, S4] * 3), 510),
    ("forty_sentences_510", " ".join([S1, S2, S3, S4] * 10), 510),
    ("clauses_only_510", ", ".join([S1.rstrip(".")] * 30), 510),
    ("words_only_510", " ".join(["bˈɪld"] * 200), 510),
    ("unbroken_run_510", LONG_WORD, 510),
    ("mixed_small_40", " ".join([S1, S2, S3]), 40),
    ("mixed_small_64", " ".join([S2, S3, S4, S1]), 64),
    ("clause_small_20", "aa, bbb; cc: dddd, eeeeeeeeeeeeeeeeeeeeeeeeeee ff.", 20),
    ("run_small_7", "abcdefghijklmnopqrstu v", 7),
    ("newlines", S1 + "\n\n" + S2 + "\n" + S3, 60),
    ("ellipsis_and_marks", "wˈAt… ɹˈIt? jˈɛs! nˈO. " * 20, 50),
]


def main() -> None:
    out = []
    for name, text, limit in CASES:
        normalized = " ".join(text.split())
        batches = split_phonemes(normalized, limit)
        out.append({
            "name": name,
            "input": text,
            "max_length": limit,
            "normalized": normalized,
            "batches": batches,
            "pauses": [pause_after(b, 0.25, 0.1) for b in batches],
        })
    path = pathlib.Path(__file__).with_name("chunk_golden.json")
    path.write_text(json.dumps(out, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
