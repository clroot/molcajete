// Real video bitstreams for the browser tests: encoded with ffmpeg, split
// into frames and framed like molcajeted does (molcajete_protocol::video).
import { execFileSync } from "node:child_process";

/** Encodes `colors` (one second each at 10 fps) as an elementary stream. */
export function encode(codec, colors) {
  const inputs = colors.flatMap((c) => ["-f", "lavfi", "-i", `color=${c}:size=64x64:rate=10:duration=0.3`]);
  const concat = colors.map((_, i) => `[${i}:v]`).join("") + `concat=n=${colors.length}:v=1:a=0[v]`;
  const common = ["-hide_banner", "-loglevel", "error", ...inputs, "-filter_complex", concat, "-map", "[v]", "-pix_fmt", "yuv420p"];
  const args = {
    h264: [...common, "-c:v", "libx264", "-bf", "0", "-x264-params", "aud=1", "-f", "h264", "-"],
    av1: [...common, "-c:v", "libsvtav1", "-preset", "12", "-f", "obu", "-"],
  }[codec];
  return execFileSync("ffmpeg", args, { maxBuffer: 1 << 26, stdio: ["ignore", "pipe", "ignore"] });
}

/** Splits an Annex B stream at access unit delimiters (NAL type 9). */
export function splitH264(buf) {
  const starts = [];
  for (let i = 0; i + 4 < buf.length; i++) {
    if (buf[i] === 0 && buf[i + 1] === 0 && buf[i + 2] === 0 && buf[i + 3] === 1 && (buf[i + 4] & 0x1f) === 9) starts.push(i);
  }
  return starts.map((s, k) => buf.subarray(s, starts[k + 1] ?? buf.length));
}

/** Splits AV1 low-overhead OBUs into temporal units (OBU type 2 starts one). */
export function splitAv1(buf) {
  const units = [];
  let i = 0;
  let start = 0;
  while (i < buf.length) {
    const header = buf[i];
    const type = (header >> 3) & 0xf;
    const hasExt = (header >> 2) & 1;
    let j = i + 1 + hasExt;
    let size = 0;
    for (let shift = 0; ; shift += 7) {
      const b = buf[j++];
      size |= (b & 0x7f) << shift;
      if (!(b & 0x80)) break;
    }
    if (type === 2 && i > start) {
      units.push(buf.subarray(start, i));
      start = i;
    }
    i = j + size;
  }
  units.push(buf.subarray(start));
  return units;
}

/** molcajete_protocol::video framing. */
export function frame(payload, timestampMs, key) {
  const out = Buffer.alloc(9 + payload.length);
  out.writeUInt32BE(payload.length, 0);
  out.writeUInt32BE(timestampMs, 4);
  out[8] = key ? 1 : 0;
  payload.copy(out, 9);
  return out;
}
