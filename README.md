> [!NOTE]
> This project is a rewrite of the ErsatzTV streaming engine in Rust. It is the default streaming engine in [ErsatzTV (legacy)](https://github.com/ErsatzTV/legacy), and can also run standalone. Configuration and playout formats are versioned, and breaking changes are called out in the [changelog](CHANGELOG.md).

# ErsatzTV

ErsatzTV is a modular, self-hosted IPTV server that transcodes and streams your media as live TV channels.

## Background

This rewrite focuses on a "one thing well" philosophy: **reliable transcoding and streaming**.

> [!IMPORTANT]
> Library and metadata management, scheduling and playout creation **are not in scope for this project**.

Unlike [the legacy version](https://github.com/ErsatzTV/legacy), this version is decoupled from library management and scheduling. It consumes **playouts** (JSON documents describing what to play and when) and handles the heavy lifting of keeping a stream alive and consistent, regardless of source media variations.

### Ways to use it

- **With ErsatzTV (legacy):** channels use this engine by default (**Streaming Engine** set to **Next**). Legacy keeps handling libraries and scheduling, and writes the playout and channel configuration that this engine consumes. No separate install is needed.
- **Standalone:** bring your own scheduler (or hand-written playouts) that writes JSON matching the [schemas](schema), and run the `ersatztv` server described below.

## Features

- **Hardware acceleration:** AMF, CUDA, QSV, RKMPP, VAAPI, VideoToolbox and Vulkan. Capabilities are probed at runtime, and anything the hardware can't do falls back to software.
- **Normalization:** every item is transcoded to the channel's configured format, resolution and audio layout, including HDR10 and Dolby Vision tonemapping, deinterlacing, and anamorphic and rotated sources.
- **Stream copy:** channels can copy video and/or audio instead of transcoding when the source already matches.
- **Overlays:** watermarks, graphics layers and burned-in subtitles.
- **Sources:** local files, HTTP and RTSP streams, commands that write MPEG-TS to stdout, and dynamic items that are resolved over HTTP at playback time.
- **Gap filling:** schedule gaps, missing playouts and items that fail to transcode are replaced with a fallback stream instead of dropping the channel.
- **IPTV output:** HLS per channel, an M3U channel list, and XMLTV guide data.

## Documentation

### Fork: HLS audio handoffs

At a change of program or fallback, this fork keeps the next video's existing timestamp
anchor and rejects incoming audio packets that would overlap audio already emitted. It
uses FFmpeg's `noise` bitstream filter with byte mutation disabled and a timestamp-based
drop expression. The remaining audio keeps its original timestamps; no codec-delay
constant, audio re-encode for copied streams, or schedule adjustment is added.

The bundled FFmpeg supports the required `noise` bitstream filter and its `amount`
and `drop` options. A custom FFmpeg override must provide those capabilities too.

This drops whole packets at the incoming boundary. It can remove a small part of the next
item's audio and leave a gap shorter than one incoming audio packet. It is not a crossfade
or sample-accurate splice. The worker also marks MPEG-TS continuity-counter resets when
starting each FFmpeg job, even when timestamps continue across that boundary.

The implementation is in `crates/ersatztv-channel/src/pts_scanner.rs` (separate video/audio
ends), `crates/ffpipeline/src/output_option.rs` (packet admission), and
`crates/ffpipeline/src/output_format.rs` (transport signaling). It applies to ordinary
scheduled items and fallback transitions, independently of the scheduler or provider.

Quickstart guides and reference documentation live at **<https://ersatztv.org/next-docs/>**.

## Contents

This project contains the following crates:

- [ffpipeline](crates/ffpipeline): transcoding and normalization logic
- [ersatztv-playout](crates/ersatztv-playout): Rust models for the playout JSON schema
- [ersatztv-channel](crates/ersatztv-channel): generates a normalized IPTV stream for a single channel from playout JSON
- [ersatztv](crates/ersatztv): serves IPTV over HTTP (M3U, M3U8, XMLTV) and manages channel processes
- [ersatztv-core](crates/ersatztv-core): shared utilities, including config merging and schema versioning
- [ersatztv-playout-generator](crates/ersatztv-playout-generator): generates playout JSON from a folder of video files. *Provided for demonstration and reference purposes; scheduling is not in scope and feature requests will not be accepted.*
- `lib*-sys` (e.g. [libva-sys](crates/libva-sys), [libvpl-sys](crates/libvpl-sys)): FFI bindings used to probe hardware acceleration capabilities

The JSON schemas under [schema](schema) are the public contract for integrators: [playout.json](schema/playout.json), [channel_config.json](schema/channel_config.json) and [lineup_config.json](schema/lineup_config.json). Each document carries a `version`; files with an incompatible version are rejected with an error rather than misread.

Finally, there are configuration examples under [examples](examples):

- [playout.json](examples/playout/playout.json): an example playout JSON file, demonstrating some of the possible fields.
- [channel.json](examples/channel.json): an example channel configuration, linking a channel to its playout JSON files, and describing how to normalize the content.
- [channel_copy.json](examples/channel_copy.json): an example channel configuration that copies video and audio when the source format allows it.
- [lineup.json](examples/lineup.json): an example lineup configuration, linking to all channels, and describing where to write the normalized content and how to serve it over HTTP.

## Fork option: VAAPI HDR10 LUT tone mapping

Set `normalization.video.hdr_vaapi_lut` to `true` in `channel.json` to prefer
`tonemap_vaapi_lut` for 8-bit output. This ports the Tunarr fork's optional
fixed-Hable lookup-table conversion to the ordinary VAAPI pipeline; CRT is not
required. The filter runs on VAAPI surfaces, avoiding the libplacebo path's
Vulkan handoffs.

Selection requires known 10-bit input, limited (`tv`) range, BT.2020 primaries,
BT.2020 non-constant-luminance matrix, PQ transfer, and no Dolby Vision profile.
Frames must already be on VAAPI in P010 format; other surface/format paths keep
the existing mapper. The patched FFmpeg must provide `tonemap_vaapi_lut`. When the option is unset,
the filter is absent, or the input/output is outside that scope, the existing
mapping path remains in use. A requested but unavailable filter logs one warning
at worker startup. This option converts HDR to SDR; it does not disable
tone mapping or preserve HDR.

The fixed 1000-nit Hable curve can render highlights and brightness differently
from libplacebo. Compare the picture on your media before enabling it. The
selection and surface handling live in `crates/ffpipeline/src/accel/vaapi.rs`;
`pipeline.rs` checks the original probe metadata before HDR classification loses
range and Dolby Vision detail. The CRT shader keeps its existing, separate HDR
eligibility check; this option does not change that path. This fork extension keeps the upstream channel
schema URI; integrators must use a fork worker that supports the field.

The default is `false`. The preference only chooses the mapper when HDR is being
converted to SDR; it has no effect when video is copied or HDR is preserved.

## Fork option: HDR tone mapping policy

Set `normalization.video.tone_mapping` in `channel.json`:

- `"on"` (default): convert HDR to SDR, preserving existing behavior.
- `"sdr_fallback"`: preserve compatible HDR. If the item cannot be preserved
  (Dolby Vision profile 5 or HDR with effective 8-bit output), convert it to SDR
  and report `hdr_tone_mapped` with the reason.
- `"off"`: never tone-map. An incompatible HDR item reports
  `hdr_preservation_unsupported` and uses the configured error/fallback picture.

Preserving HDR requires 10-bit output and an HDR-capable player/display. SDR
programs can still use 8-bit profiles in any mode. These decisions use the actual
source and effective output, including MPEG-2's 8-bit limit. Copied video retains
its existing behavior. Dolby Vision metadata preservation is not promised.
SDR-only custom shaders are skipped while HDR is preserved.

Reasons are written to `ffmpeg.degradation_report` for the scheduled item. Tunarr
exposes them in its existing sessions status APIs; no new status UI is included.
`hdr_vaapi_lut` only selects a conversion method when this policy actually maps
HDR to SDR. Changing the configuration applies to a new worker session.

This fork extension keeps upstream channel schema URI `0.1.3`; use a compatible
fork worker. The bundled Tunarr worker is pinned, but an explicit worker-path
override must also support these fields. HDR signaling, display switching and
subtitle brightness on physical clients remain release checks; producing tagged
10-bit segments alone does not establish client compatibility.

## Getting Started

### Prerequisites

- **ErsatzTV's ffmpeg build.** Use `ffmpeg` and `ffprobe` from [ErsatzTV-ffmpeg](https://github.com/ErsatzTV/ErsatzTV-ffmpeg/releases/latest), either in your `PATH` or referenced by absolute path in `channel.json`. It carries patches and fixes that this project relies on; stock or distro ffmpeg builds are not supported. The Docker image already includes it.

### Install

- **Docker:** `ersatztv/next:latest` (also `ghcr.io/ersatztv/next:latest`) for `linux/amd64` and `linux/arm64`. Use a release-line tag such as `:v0.2` to get fixes and features without breaking changes.
- **Binaries:** download a build for Windows, Linux (x64, x64 musl, arm64) or macOS (x64, arm64) from the [latest release](https://github.com/ErsatzTV/next/releases/latest).
- **Development builds:** binaries from `main` are in [ErsatzTV/next-develop-builds](https://github.com/ErsatzTV/next-develop-builds/releases/latest), and images are tagged `:develop`.
- **Source:** `cargo build --release --workspace`.

Releases use semantic versioning; before 1.0, `0.B.C` bumps `B` for breaking changes. Each release's notes list the config versions it reads and the ErsatzTV-ffmpeg build it was tested with. See [Releases](https://ersatztv.org/next-docs/releases) for the details and every Docker tag.

### Quick Start

1. **Scaffold a lineup with one channel:**
   ```bash
   ersatztv add-lineup config/lineup.json --channels 1
   ```
   This creates `config/lineup.json`, `config/hls/`, `config/channels/1/channel.json`, and `config/channels/1/playout/`.

2. **Generate a test playout** from a folder of video files:
   ```bash
   ersatztv-playout-generator --lineup config/lineup.json --channel 1 --content-folder /path/to/videos
   ```

3. **Run the server:**
   ```bash
   ersatztv config/lineup.json
   ```

4. **Watch** at `http://localhost:8409/channel/1.m3u8` in VLC, mpv, or any HLS player. For a no-install check, open the [hls.js demo](https://hlsjs.video-dev.org/demo/?src=http%3A%2F%2Flocalhost%3A8409%2Fchannel%2F1.m3u8).

   IPTV clients can use the channel list at `http://localhost:8409/channels.m3u` and guide data at `http://localhost:8409/xmltv.xml`.

### Quick Start (Docker)

The image runs `ersatztv /config/lineup.json`. Media paths in your playouts must be valid inside the container, so mount your media at the same path you use in the playouts:

```bash
# scaffold
docker run --rm -v ./config:/config ersatztv/next:latest add-lineup /config/lineup.json --channels 1

# generate a test playout
docker run --rm -v ./config:/config -v /path/to/videos:/path/to/videos \
  --entrypoint /app/ersatztv-playout-generator ersatztv/next:latest \
  --lineup /config/lineup.json --channel 1 --content-folder /path/to/videos

# run the server
docker run -d -p 8409:8409 -v ./config:/config -v /path/to/videos:/path/to/videos ersatztv/next:latest
```

For hardware acceleration, pass the device through: `--device /dev/dri` for VAAPI/QSV, or `--gpus all` for NVIDIA.

## Reporting Issues

When a stream fails, the most useful report includes:

- the output of `ersatztv-channel debug <path/to/channel.json>`, which logs the merged channel config, the ffmpeg build features and the hardware capabilities this engine detected;
- an error dossier: set `ffmpeg.reports_folder` in `channel.json`, and each failed transcode writes a folder containing the ffmpeg report and stderr, the generated pipeline, the playout item, media info and the channel config.

## Contributing

We welcome feedback and contributions!

- **Matrix:** [#ersatztv-dev:matrix.org](https://matrix.to/#/#ersatztv-dev:matrix.org)
- **Discord:** [#developer-chat](https://discord.ersatztv.org)

Early feedback on the **playout schema** and architecture is especially valuable at this stage.

## License

ErsatzTV is licensed under the [MIT License](LICENSE).

## Fork option: preserve program aspect ratio

Set `normalization.video.preserve_source_aspect` to `true` to send each
transcoded program at its own display shape without baking in channel-sized
black bars. The default is `false`, retaining the existing scaling policy.
The option overrides `scaling_mode` and requires configured output dimensions;
copied video is unchanged.

For example, a 4:3 program on a 1920x1080 channel remains encoded at 1920x1080
but is tagged for 4:3 display. A player can fill a 4:3 screen, or add its own
side bars on a widescreen display. A widescreen program still displays wide.
This cannot remove black bars already present in the source recording.

The pipeline composes the picture, subtitles, watermark and credit squeeze in
a square-pixel canvas fitted to the source display aspect, then stretches the
completed image to the fixed encoded raster and sets its display aspect.
Custom full-frame graphics should be authored for the program's display shape.
Even-pixel canvas rounding can slightly approximate unusual aspect ratios.

Requires a fork worker release containing this option (not present in
`v0.2.4-trfork14`). The upstream channel schema URI remains `0.1.3`. Integrators
must pin a supporting release; an older worker can silently ignore this field.
Players and output adapters must honor aspect changes at program boundaries.
Fixed encoded dimensions alone do not guarantee compatibility with every TV
or HLS client; keep the default for clients that require a fixed display shape.
