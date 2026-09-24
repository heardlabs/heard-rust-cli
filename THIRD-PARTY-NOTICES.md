# Third-party notices

heard-rust-cli is licensed under the Apache License 2.0 (see `LICENSE`). It
builds on the following third-party work. Rust crate dependencies and their
licences are listed in `Cargo.lock`; all are permissive (MIT, Apache-2.0, BSD,
ISC, Zlib, Unicode, or CDLA-Permissive-2.0 — the Mozilla CA root data in
`webpki-roots`).

## Voice model — downloaded at setup, not bundled

| Component | Licence | Source |
|---|---|---|
| Kokoro-82M (model weights and voices) | Apache-2.0 | https://huggingface.co/hexgrad/Kokoro-82M |
| kokoro-onnx model files (`kokoro-v1.0.onnx`, `voices-v1.0.bin`) | MIT | https://github.com/thewh1teagle/kokoro-onnx |

## Linked into the binary

| Component | Licence | Notes |
|---|---|---|
| misaki English G2P, lexicons and rules (`sayd-misaki-en`) | Apache-2.0 | Grapheme-to-phoneme for Kokoro; replaces espeak-ng (GPL), which is not used. |
| ONNX Runtime (via the `ort` crate) | MIT | Runs the Kokoro model on the CPU. |
| `ort` | MIT OR Apache-2.0 | |

## Ported source

| Component | Licence | Notes |
|---|---|---|
| PyYAML 6 (`yaml.safe_load` / `yaml.safe_dump`, pure-Python loader and dumper) | MIT | `crates/heard-config/src/pyyaml/` is a Rust port of its reader, scanner, parser, composer, constructor, representer, serializer and emitter. Copyright (c) 2017-2021 Ingy döt Net; Copyright (c) 2006-2016 Kirill Simonov. https://github.com/yaml/pyyaml |

PyYAML's licence (MIT):

```text
Copyright (c) 2017-2021 Ingy döt Net
Copyright (c) 2006-2016 Kirill Simonov

Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies
of the Software, and to permit persons to whom the Software is furnished to do
so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

No GPL or LGPL component is linked or distributed.
