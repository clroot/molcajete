// File transfer: Guacamole filesystem objects, directory listings, uploads
// and downloads. Runs where the wasm client runs (the worker, or the page
// without OffscreenCanvas), so file data never passes through the page in
// worker mode.
//
// - Uploads read the File in slices (File.slice), so a file is never read
//   into memory at once, and keep a few blobs in flight, waiting for the
//   server's acknowledgements before sending more.
// - Downloads are written as they arrive: to a file the user picked
//   (File System Access, Chromium), or collected into a Blob that the page
//   saves at the end. Blobs are acknowledged only as fast as they are
//   written, so the server never runs ahead of the disk.
//
// Events out (through `emit`):
//   filesystem {object, name, canDownload, canUpload}
//   filesystem-removed {object}
//   transfer {id, direction, name, state, bytes, total, error?, blob?}
//     state: "active", "done", "failed" or "cancelled"; `blob` with the
//     last event of a download collected in memory

export const INDEX_MIMETYPE = "application/vnd.glyptodon.guacamole.stream-index+json";

/** Raw bytes per upload blob: 16 KiB, 21848 bytes of base64, within guacd's limits. */
const UPLOAD_BLOB = 16 * 1024;
/** Upload blobs in flight: more fills a slow link's round trip, less keeps input ahead of file data. */
const UPLOAD_WINDOW = 16;
/** How much of a file is read at once for uploading. */
const UPLOAD_SLICE = 1024 * 1024;
/** Uploads running at once; the rest wait. */
const MAX_UPLOADS = 2;
/** Downloaded data waiting to be written before acknowledgements are held back. */
const DOWNLOAD_BUFFER = 8 * 1024 * 1024;
/** Downloaded data is written in pieces of about this size. */
const WRITE_SIZE = 1024 * 1024;
/** A download collected in memory is folded into a Blob every so often, which browsers may keep on disk. */
const BLOB_FOLD = 64 * 1024 * 1024;
/** Progress events per transfer at most this often. */
const PROGRESS_INTERVAL = 250;

const SUCCESS = 0x0000;
const CLIENT_ABORT = 0x0308; // CLIENT_TIMEOUT: the stock client's code for giving up on a stream

/** The size a molcajeted listing gives as a mimetype parameter, if any. */
function sizeOf(mimetype) {
  const match = /;\s*size=(\d+)/.exec(mimetype ?? "");
  return match ? Number(match[1]) : null;
}

function baseName(path) {
  const parts = path.split("/").filter(Boolean);
  return parts.length ? parts[parts.length - 1] : "/";
}

/** Entries of a JSON stream index, directories first, then by name. */
export function parseIndex(text) {
  const index = JSON.parse(text);
  const entries = Object.entries(index).map(([path, mimetype]) => {
    const isDirectory = mimetype === INDEX_MIMETYPE;
    return {
      path,
      name: baseName(path),
      type: isDirectory ? "directory" : "file",
      size: isDirectory ? null : sizeOf(mimetype),
    };
  });
  entries.sort((a, b) =>
    a.type === b.type ? a.name.localeCompare(b.name) : a.type === "directory" ? -1 : 1,
  );
  return entries;
}

/** Writes downloaded data to a file the user picked. */
class FileSink {
  #writable;

  constructor(handle) {
    this.ready = handle.createWritable().then((writable) => {
      this.#writable = writable;
    });
  }

  write(chunk) {
    return this.#writable.write(chunk);
  }

  async close() {
    await this.#writable.close();
    return null;
  }

  abort() {
    this.#writable?.abort().catch(() => {});
  }
}

/** Collects downloaded data into a Blob. */
class BlobSink {
  ready = Promise.resolve();
  #blobs = [];
  #parts = [];
  #partBytes = 0;

  constructor(mimetype) {
    this.mimetype = mimetype;
  }

  write(chunk) {
    this.#parts.push(chunk);
    this.#partBytes += chunk.byteLength;
    if (this.#partBytes >= BLOB_FOLD) this.#fold();
    return Promise.resolve();
  }

  #fold() {
    this.#blobs.push(new Blob(this.#parts));
    this.#parts = [];
    this.#partBytes = 0;
  }

  async close() {
    this.#fold();
    const blob = new Blob(this.#blobs, { type: this.mimetype });
    this.#blobs = [];
    return blob;
  }

  abort() {
    this.#blobs = [];
    this.#parts = [];
  }
}

export class FileTransfers {
  #client;
  #emit;
  /** Requests waiting for their stream, by `${object}:${name}`: listings and downloads. */
  #gets = new Map();
  /** Incoming streams, by stream index. */
  #incoming = new Map();
  /** Uploads, by stream index once open. */
  #uploads = new Map();
  #queuedUploads = [];
  #transfers = new Map();

  /**
   * @param client the wasm MolcajeteClient
   * @param emit (type, detail, transferables?) delivers events to the page
   */
  constructor(client, emit) {
    this.#client = client;
    this.#emit = emit;
  }

  /** Handles the client's file events; returns whether `type` was one. */
  onEvent(type, detail) {
    switch (type) {
      case "filesystem":
        this.#emit("filesystem", {
          object: detail.object,
          name: detail.name,
          // Stock guacd does not say; it may refuse when asked.
          canDownload: detail.download !== false,
          canUpload: detail.upload !== false,
        });
        return true;
      case "undefine":
        this.#emit("filesystem-removed", { object: detail.object });
        return true;
      case "get-error":
        this.#getFailed(detail);
        return true;
      case "stream-start":
        this.#streamStart(detail);
        return true;
      case "stream-data":
        this.#incoming.get(detail.stream)?.data(detail.data);
        return true;
      case "stream-end":
        this.#incoming.get(detail.stream)?.end(detail.failed === true);
        this.#incoming.delete(detail.stream);
        return true;
      case "stream-ack":
        this.#uploads.get(detail.stream)?.ack(detail.code, detail.message);
        return true;
      case "state":
        // A new or closed tunnel ends every transfer.
        if (detail === "connecting" || detail === "disconnected") this.#reset("The connection closed");
        return false;
      default:
        return false;
    }
  }

  #reset(reason) {
    for (const pending of this.#gets.values()) for (const request of pending) request.fail(reason);
    this.#gets.clear();
    for (const stream of this.#incoming.values()) stream.end(true, reason);
    this.#incoming.clear();
    for (const upload of [...this.#uploads.values(), ...this.#queuedUploads]) upload.fail(reason);
    this.#uploads.clear();
    this.#queuedUploads = [];
  }

  #request(object, name, request) {
    const key = `${object}:${name}`;
    const pending = this.#gets.get(key) ?? [];
    pending.push(request);
    this.#gets.set(key, pending);
    this.#client.get(object, name);
  }

  #take(object, name) {
    const key = `${object}:${name}`;
    const pending = this.#gets.get(key);
    const request = pending?.shift();
    if (pending && !pending.length) this.#gets.delete(key);
    return request;
  }

  #getFailed({ object, name, message }) {
    this.#take(object, name)?.fail(message || "The server refused");
  }

  #streamStart({ stream, object, mimetype, name }) {
    const request = object === null ? null : this.#take(object, name);
    if (!request) {
      // A file the server offers by itself (`file`), or one nobody asked for.
      this.#client.ack_stream(stream, "Not wanted", CLIENT_ABORT);
      return;
    }
    const handler = request.start(stream, mimetype);
    if (!handler) {
      this.#client.ack_stream(stream, "Not wanted", CLIENT_ABORT);
      return;
    }
    this.#incoming.set(stream, handler);
    this.#client.ack_stream(stream, "OK", SUCCESS);
  }

  /** Lists the directory `path` of filesystem `object`. */
  list(object, path) {
    return new Promise((resolve, reject) => {
      this.#request(object, path, {
        fail: (message) => reject(new Error(message)),
        start: (stream, mimetype) => {
          if (mimetype !== INDEX_MIMETYPE) {
            reject(new Error("Not a directory"));
            return null;
          }
          const parts = [];
          return {
            data: (chunk) => {
              parts.push(chunk);
              this.#client.ack_stream(stream, "OK", SUCCESS);
            },
            end: (failed, reason) => {
              if (failed) {
                reject(new Error(reason ?? "The listing was cut short"));
                return;
              }
              try {
                resolve(parseIndex(new TextDecoder().decode(parts.length ? concat(parts) : new Uint8Array())));
              } catch (err) {
                reject(err);
              }
            },
          };
        },
      });
    });
  }

  #progress(transfer, state, extra = {}, transferables = []) {
    const now = performance.now();
    if (state === "active" && now - transfer.reported < PROGRESS_INTERVAL) return;
    transfer.reported = now;
    this.#emit(
      "transfer",
      {
        id: transfer.id,
        direction: transfer.direction,
        name: transfer.name,
        path: transfer.path,
        state,
        bytes: transfer.bytes,
        total: transfer.total,
        ...extra,
      },
      transferables,
    );
    if (state !== "active") this.#transfers.delete(transfer.id);
  }

  /**
   * Downloads `path` of filesystem `object`. `sink` is `{handle}`, a
   * FileSystemFileHandle to write to, or `{blob: true}` to collect the
   * file into a Blob, delivered with the last `transfer` event.
   */
  download(id, object, path, sink, size = null) {
    const transfer = {
      id,
      direction: "download",
      name: baseName(path),
      path,
      bytes: 0,
      total: size,
      reported: 0,
      cancel: () => {},
    };
    this.#transfers.set(id, transfer);
    this.#progress(transfer, "active");
    const fail = (message) => this.#progress(transfer, "failed", { error: message });
    this.#request(object, path, {
      fail,
      start: (stream, mimetype) => {
        if (mimetype === INDEX_MIMETYPE) {
          fail("Folders cannot be downloaded");
          return null;
        }
        transfer.total = sizeOf(mimetype) ?? transfer.total;
        const target = sink.handle ? new FileSink(sink.handle) : new BlobSink(mimetype.split(";")[0]);
        return this.#receive(transfer, stream, target);
      },
    });
  }

  /** Writes an incoming stream to `target`, acknowledging as it keeps up. */
  #receive(transfer, stream, target) {
    let queue = [];
    let queued = 0;
    let unwritten = 0;
    let heldAcks = 0;
    let writing = false;
    let ended = false;
    let stopped = false;
    const client = this.#client;

    const stop = (state, error) => {
      if (stopped) return;
      stopped = true;
      target.abort();
      this.#progress(transfer, state, error ? { error } : {});
    };
    transfer.cancel = () => {
      if (stopped) return;
      client.ack_stream(stream, "Cancelled", CLIENT_ABORT);
      this.#incoming.delete(stream);
      stop("cancelled");
    };

    const finish = async () => {
      try {
        const blob = await target.close();
        if (blob) this.#progress(transfer, "done", { blob });
        else this.#progress(transfer, "done");
      } catch (err) {
        this.#progress(transfer, "failed", { error: String(err?.message ?? err) });
      }
      stopped = true;
    };

    const flush = async () => {
      if (writing || stopped) return;
      writing = true;
      try {
        await target.ready;
        while (queue.length && !stopped) {
          const parts = queue;
          queue = [];
          queued = 0;
          const chunk = concat(parts);
          await target.write(chunk);
          unwritten -= chunk.byteLength;
          transfer.bytes += chunk.byteLength;
          this.#progress(transfer, "active");
          while (heldAcks > 0 && unwritten < DOWNLOAD_BUFFER) {
            heldAcks--;
            client.ack_stream(stream, "OK", SUCCESS);
          }
        }
      } catch (err) {
        client.ack_stream(stream, "Write failed", CLIENT_ABORT);
        this.#incoming.delete(stream);
        stop("failed", String(err?.message ?? err));
        return;
      } finally {
        writing = false;
      }
      if (ended && !stopped) finish();
    };

    return {
      data: (chunk) => {
        if (stopped) return;
        queue.push(chunk);
        queued += chunk.byteLength;
        unwritten += chunk.byteLength;
        if (unwritten < DOWNLOAD_BUFFER) client.ack_stream(stream, "OK", SUCCESS);
        else heldAcks++;
        if (queued >= WRITE_SIZE) flush();
      },
      end: (failed, reason) => {
        if (stopped) return;
        if (failed) {
          stop("failed", reason ?? "The download was cut short");
          return;
        }
        ended = true;
        if (queue.length || writing) flush();
        else finish();
      },
    };
  }

  /**
   * Uploads `file` into filesystem `object` at `path`, or with no object
   * into the server's upload directory.
   */
  upload(id, file, object = null, path = null) {
    const upload = new Upload(this, id, file, object, path);
    this.#transfers.set(id, upload.transfer);
    this.#progress(upload.transfer, "active");
    this.#queuedUploads.push(upload);
    this.#startUploads();
  }

  #startUploads() {
    while (this.#uploads.size < MAX_UPLOADS && this.#queuedUploads.length) {
      const upload = this.#queuedUploads.shift();
      const stream = this.#client.open_upload(
        upload.object ?? -1,
        upload.file.type || "application/octet-stream",
        upload.path ?? upload.file.name,
      );
      if (stream < 0) {
        upload.fail("Not connected");
        continue;
      }
      upload.stream = stream;
      this.#uploads.set(stream, upload);
    }
  }

  /** @internal for Upload */
  uploadEnded(upload, state, error) {
    if (upload.stream !== null) this.#uploads.delete(upload.stream);
    this.#queuedUploads = this.#queuedUploads.filter((u) => u !== upload);
    this.#progress(upload.transfer, state, error ? { error } : {});
    this.#startUploads();
  }

  /** @internal for Upload */
  uploadProgress(upload) {
    this.#progress(upload.transfer, "active");
  }

  /** @internal for Upload */
  get client() {
    return this.#client;
  }

  cancel(id) {
    this.#transfers.get(id)?.cancel();
  }
}

class Upload {
  stream = null;
  #files;
  #opened = false;
  #ended = false;
  #inFlight = 0;
  #sent = 0;
  #slice = null;
  #sliceOffset = 0;
  #reading = false;

  constructor(files, id, file, object, path) {
    this.#files = files;
    this.file = file;
    this.object = object;
    this.path = path;
    this.transfer = {
      id,
      direction: "upload",
      name: file.name,
      path: path ?? file.name,
      bytes: 0,
      total: file.size,
      reported: 0,
      cancel: () => this.#cancel(),
    };
  }

  ack(code, message) {
    if (this.#ended) return;
    if (code !== SUCCESS) {
      this.#end("failed", message || "The server refused the file");
      return;
    }
    if (!this.#opened) {
      this.#opened = true;
    } else {
      this.#inFlight--;
      this.transfer.bytes = Math.min(this.file.size, this.transfer.bytes + UPLOAD_BLOB);
      this.#files.uploadProgress(this);
    }
    this.#pump();
  }

  async #pump() {
    if (this.#reading || this.#ended) return;
    this.#reading = true;
    try {
      const client = this.#files.client;
      while (this.#inFlight < UPLOAD_WINDOW && this.#sent < this.file.size && !this.#ended) {
        if (!this.#slice || this.#sliceOffset >= this.#slice.byteLength) {
          const end = Math.min(this.file.size, this.#sent + UPLOAD_SLICE);
          this.#slice = new Uint8Array(await this.file.slice(this.#sent, end).arrayBuffer());
          this.#sliceOffset = 0;
          if (this.#ended) return;
        }
        const chunk = this.#slice.subarray(this.#sliceOffset, this.#sliceOffset + UPLOAD_BLOB);
        client.upload_blob(this.stream, chunk);
        this.#sliceOffset += chunk.byteLength;
        this.#sent += chunk.byteLength;
        this.#inFlight++;
      }
      if (this.#sent >= this.file.size && this.#inFlight === 0 && !this.#ended) {
        this.transfer.bytes = this.file.size;
        client.end_upload(this.stream);
        this.#end("done");
      }
    } catch (err) {
      // The file could not be read (moved, deleted, permission revoked).
      this.#files.client.end_upload(this.stream);
      this.#end("failed", String(err?.message ?? err));
    } finally {
      this.#reading = false;
    }
  }

  #cancel() {
    if (this.#ended) return;
    if (this.stream !== null) this.#files.client.end_upload(this.stream);
    this.#end("cancelled");
  }

  fail(reason) {
    this.#end("failed", reason);
  }

  #end(state, error) {
    if (this.#ended) return;
    this.#ended = true;
    this.#slice = null;
    this.#files.uploadEnded(this, state, error);
  }
}

function concat(parts) {
  if (parts.length === 1) return parts[0];
  const total = parts.reduce((sum, part) => sum + part.byteLength, 0);
  const joined = new Uint8Array(total);
  let offset = 0;
  for (const part of parts) {
    joined.set(part, offset);
    offset += part.byteLength;
  }
  return joined;
}
