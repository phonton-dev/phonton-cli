/**
 * Note: When using the Node.JS APIs, the config file
 * doesn't apply. Instead, pass options directly to the APIs.
 *
 * All configuration options: https://remotion.dev/docs/config
 */

import { Config } from "@remotion/cli/config";

Config.setRspack(true);
Config.setVideoImageFormat("jpeg");
Config.setOverwriteOutput(true);
// Low-RAM laptop: x264 with one thread per core (24) failed to allocate.
// Two encoder threads; the last ffmpeg argument is the output file.
if (process.platform === "win32") {
  Config.overrideFfmpegCommand(({ args }) => [...args.slice(0, -1), "-threads", "2", ...args.slice(-1)]);
}
