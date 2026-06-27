# Third-Party Notices

## pi

The agent core of **pixie-pi** — the agent loop, Anthropic Messages SSE streaming,
tool execution, fuzzy edit matching, LLM-summarizing compaction, streaming-JSON
repair, and truncation — is a from-scratch reimplementation in Rust of the
design first implemented in TypeScript as **pi**
(<https://github.com/earendil-works/pi>) by Mario Zechner.

No source code from `pi` is included in this repository; pixie-pi was written
independently in a different language. `pi` is licensed under the MIT License,
reproduced below in full out of an abundance of caution and in recognition of
its foundational design work.

---

    MIT License

    Copyright (c) 2025 Mario Zechner

    Permission is hereby granted, free of charge, to any person obtaining a copy
    of this software and associated documentation files (the "Software"), to deal
    in the Software without restriction, including without limitation the rights
    to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
    copies of the Software, and to permit persons to whom the Software is
    furnished to do so, subject to the following conditions:

    The above copyright notice and this permission notice shall be included in all
    copies or substantial portions of the Software.

    THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
    IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
    FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
    AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
    LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
    OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
    SOFTWARE.
