# Detection benchmark

Measures how public bot-detection services see each candidate browser engine when
it runs **headful on a headless Wayland compositor** (sway, `WLR_BACKENDS=headless`),
the way the workstation runtime ships it. Used to choose the default engine.

Engines (`--engine`):

| name | what it is | identity mechanism |
|---|---|---|
| `fpc` | [fingerprint-chromium](https://github.com/adryfish/fingerprint-chromium) (ungoogled-chromium fork) | `--fingerprint=<seed>` (C++), `--fingerprint-brand=Chrome`, `--timezone`, `--lang`, `--accept-lang` |
| `fpc-brand157` | same binary | additionally `--fingerprint-brand-version=<current stable Chrome>` to test the version-lag question |
| `camoufox` | [Camoufox](https://github.com/daijro/camoufox) (Firefox fork) | C++ spoofing from a generated config; generated once per seed and replayed, geoip aligned to the egress |
| `camoufox-linux` | same binary | identity pinned to the host OS (`os="linux"`), so canvas/WebGL are not covered with per-launch noise |
| `chrome-seed` | stock Google Chrome stable | launch-time only: screen size, process locale, `TZ`, CPU-affinity core count; no page hooks |
| `chrome-jshooks` | stock Google Chrome + document-start JS overrides | the common `addScriptToEvaluateOnNewDocument` approach, kept as a known-bad baseline |

Probes (`wsbench/probes.py`). Every probe only observes the page. None of them clicks, types or solves anything.

| probe | site | scored fields |
|---|---|---|
| fingerprint | example.com (own collector) | UA, client hints, cores, memory, screen, locale, timezone, canvas/WebGL/audio hashes, fonts → `fp_hash` |
| creepjs | CreepJS | `lies` (count and APIs), like-headless / headless / stealth %, FP ID, worker-scope values |
| browserscan | BrowserScan bot detection | verdict (`Normal` / `Robot` …) and flagged checks |
| pixelscan | pixelscan fingerprint check | consistent / masking, proxy, bot check, location |
| turnstile_widget | public managed-mode Turnstile demo | `pass` (token issued without interaction) / `interactive` (checkbox waiting) / `blocked` (failure or never rendered) |
| turnstile_page | public site behind a full-page managed challenge | `pass` (origin served, 2xx, no challenge DOM) / `interactive` / `blocked` |

Classification never depends on localized text. Turnstile pass is decided from the issued
token or the navigation status. "Blocked" vs "interactive" is decided from the widget's
red/green pixels.

Detector results are read after each scan finishes, through a DOM bridge, which works the
same on every engine. Chromium extraction otherwise runs in a CDP isolated world without
`Runtime.enable`.

## Run

```sh
cd bench
./setup.sh                      # user-space install: venv, Camoufox, fingerprint-chromium (sha256-pinned), Chrome, sway
. ~/.cache/wsbench/env.sh
python -m wsbench.run --out results \
  --engine fpc --engine camoufox --engine chrome-seed \
  --egress residential=socks5://127.0.0.1:1080 --egress datacenter \
  --seeds A,A,A,A,A,B,C,D --parallel 4
python -m wsbench.report results/runs.jsonl
python -m wsbench.perf --egress residential=socks5://127.0.0.1:1080 --seeds A,A   # startup + peak RSS
python -m wsbench.integration   # CDP/BiDi, a11y, insertText, tabs, Chrome-profile carry-over
```

One command reruns the full measurement (detection + perf, then the report):

```sh
./rerun.sh --egress residential=socks5://127.0.0.1:1080
```

- `--seeds`: one fresh launch (fresh profile, fresh compositor) per entry. Repeats of a seed
  measure stability across launches. Different seeds measure distinctness.
- `--egress NAME=PROXY`: proxied egress. A bare `NAME` means direct. The proxy is checked
  before and after every run. A run where it dropped, or where a probe hit a network
  error page, is kept in `runs.jsonl` with `"discarded": true` and re-run (up to 4
  attempts). The exit IP actually used is recorded per run.
- `--display x11` uses Xvfb + openbox instead of Wayland.
- Raw results are written to `results/runs.jsonl` (one object per launch) and screenshots to
  `results/shots/<run>/`. Results are not committed.

Host notes:
- Chrome needs `WSBENCH_CHROME_NO_SANDBOX=1` on hosts without unprivileged user namespaces.
- Without a GPU, every engine renders WebGL with SwiftShader (Chromium) or software GL
  (Firefox). That is a detection signal shared by all engines on such hosts.
