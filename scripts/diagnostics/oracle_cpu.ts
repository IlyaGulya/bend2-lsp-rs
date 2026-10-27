// Diagnostic-only preload. The accepted oracle installation remains unmodified.
import fs from "node:fs";
import path from "node:path";
import { createHash } from "node:crypto";
import { Session } from "node:inspector";
import { Worker } from "node:worker_threads";
import { gzipSync } from "node:zlib";

const root = import.meta.dir;
const directory = path.join(root, "profiles");
fs.mkdirSync(directory, { recursive: true });
const events = path.join(root, "capture-events.jsonl");
const record = (value: object) => fs.appendFileSync(events,
  JSON.stringify({ at: Date.now(), pid: process.pid, ...value }) + "\n");
const failure = (where: string, error: unknown) => record({ event: "error", where, error: String(error) });

function save(role: string, threadId: number, sequence: number, json: string) {
  const profile = JSON.parse(json);
  if (!Array.isArray(profile.samples) || profile.samples.length === 0) {
    record({ event: "empty", role, threadId, sequence });
    return;
  }
  const compressed = gzipSync(json, { level: 1 });
  const filename = `${role}.${process.pid}.${threadId}.${sequence}.cpuprofile.gz`;
  fs.writeFileSync(path.join(directory, filename), compressed);
  record({ event: "profile", role, threadId, sequence, file: filename,
    bytes: compressed.length, sha256: createHash("sha256").update(compressed).digest("hex"),
    rawBytes: Buffer.byteLength(json), rawSha256: createHash("sha256").update(json).digest("hex"),
    samples: profile.samples.length, startTime: profile.startTime, endTime: profile.endTime });
}

record({ event: "start", bun: Bun.version, revision: Bun.revision,
  argv: process.argv, execArgv: process.execArgv,
  environment: Object.fromEntries(Object.entries(process.env).filter(([key]) =>
    ["PATH", "SYSTEMROOT", "TMPDIR", "TMP", "TEMP", "HOME", "BEND_LIB", "BEND_HUB"].includes(key))) });

const session = new Session();
session.connect();
function post(method: string, params: object = {}): Promise<any> {
  const { promise, resolve, reject } = Promise.withResolvers<any>();
  session.post(method, params, (error, result) => error ? reject(error) : resolve(result));
  return promise;
}
await post("Profiler.enable");
await post("Profiler.setSamplingInterval", { interval: 1000 });
await post("Profiler.start");

let stopping = false;
let mainRunning = true;
let mainSequence = 0;
let mainChain = Promise.resolve();
function mainCheckpoint(): Promise<void> {
  const run = mainChain.then(async () => {
    if (!mainRunning) return;
    const { profile } = await post("Profiler.stop");
    mainRunning = false;
    save("main", 0, mainSequence++, JSON.stringify(profile));
  });
  mainChain = run.catch(error => failure("main-checkpoint", error));
  return mainChain;
}

type ProfileHandle = { stop(): Promise<string> };
type Entry = {
  threadId: number;
  handle: Promise<ProfileHandle> | null;
  chain: Promise<void>;
  stopping: boolean;
  sequence: number;
};
const entries = new Map<Worker, Entry>();
const prototype = Worker.prototype;
const originalOn = prototype.on;
const originalOnce = prototype.once;
const originalTerminate = prototype.terminate;

function track(worker: Worker) {
  if (entries.has(worker)) return;
  if (typeof worker.startCpuProfile !== "function") {
    throw Error("Pinned runtime does not expose Worker.startCpuProfile");
  }
  // The constructor installs its first listener only after its native worker exists.
  const entry: Entry = { threadId: worker.threadId, handle: null,
    chain: Promise.resolve(), stopping: false, sequence: 0 };
  entries.set(worker, entry);
  entry.handle = worker.startCpuProfile();
  entry.handle.catch(error => failure("pristine-start", error));
  record({ event: "worker-start", role: "pristine", threadId: entry.threadId });
  originalOn.call(worker, "exit", code => record({ event: "worker-exit",
    role: "pristine", threadId: entry.threadId, code }));
}

function workerCheckpoint(worker: Worker, finish: boolean): Promise<void> {
  const entry = entries.get(worker);
  if (!entry) return Promise.resolve();
  if (entry.stopping) return entry.chain;
  if (finish) entry.stopping = true;
  const run = entry.chain.then(async () => {
    if (!entry.handle) return;
    const handle = await entry.handle;
    entry.handle = null;
    save("pristine", entry.threadId, entry.sequence++, await handle.stop());
  });
  entry.chain = run.catch(error => failure("pristine-checkpoint", error));
  return entry.chain;
}

prototype.on = function (...args: Parameters<Worker["on"]>) {
  track(this);
  return originalOn.apply(this, args);
};
prototype.once = function (...args: Parameters<Worker["once"]>) {
  track(this);
  return originalOnce.apply(this, args);
};
prototype.terminate = function () {
  // Preserve termination even if diagnostic collection fails. Verification rejects
  // the recorded error; the accepted pristine owner must still be able to exit.
  return workerCheckpoint(this, true).then(() => originalTerminate.call(this));
};

// Four continuously sampled VMs exceeded the unchanged full-run deadline.
// Keep every semantic request, but expose explicit 2s-on/8s-off sampling gaps.
let sampling = true;
let timer = setTimeout(() => { void samplingWindow(); }, 2000);
timer.unref();
async function samplingWindow(): Promise<void> {
  if (stopping) return;
  sampling = !sampling;
  if (sampling) {
    const start = mainChain.then(async () => {
      if (stopping || mainRunning) return;
      await post("Profiler.start");
      mainRunning = true;
    });
    mainChain = start.catch(error => failure("main-window-start", error));
    const workers = [...entries].map(([worker, entry]) => {
      const start = entry.chain.then(async () => {
        if (stopping || entry.stopping || entry.handle) return;
        entry.handle = worker.startCpuProfile();
        await entry.handle;
      });
      entry.chain = start.catch(error => failure("pristine-window-start", error));
      return entry.chain;
    });
    await Promise.all([mainChain, ...workers]);
  } else {
    await Promise.all([mainCheckpoint(),
      ...[...entries.keys()].map(worker => workerCheckpoint(worker, false))]);
  }
  record({ event: "sampling-window", active: sampling, activeMs: 2000, pauseMs: 8000 });
  if (!stopping) {
    timer = setTimeout(() => { void samplingWindow(); }, sampling ? 2000 : 8000);
    timer.unref();
  }
}

let finalization: Promise<void> | undefined;
function finish(): Promise<void> {
  if (finalization) return finalization;
  stopping = true;
  clearTimeout(timer);
  finalization = Promise.all([mainCheckpoint(),
    ...[...entries.keys()].map(worker => workerCheckpoint(worker, true))])
    .then(() => {
      session.disconnect();
      record({ event: "finished" });
    }).catch(error => {
      failure("finish", error);
      session.disconnect();
    });
  return finalization;
}
// Additional observation only: do not replace the accepted signal handler,
// alter process.exit, write protocol output, or extend its original grace.
process.once("SIGTERM", () => { void finish(); });
process.once("SIGINT", () => { void finish(); });
process.once("beforeExit", () => { void finish(); });
