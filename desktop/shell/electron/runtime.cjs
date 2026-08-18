// The shell's one connection to a Runtime.
//
// It lives in the main process and nowhere else. A surface that opened its own
// transport would be a second path to the same state, and this project has
// already paid for one of those: the local IPC adapter carried a check the
// network path did not, so the network path's gap stayed invisible until
// something finally used it.
//
// The proto is loaded from `contracts/proto/runtime.proto` — the same file the
// Rust runtime compiles. Two languages generating from one contract is what
// makes "protocol neutral" a fact rather than a claim.
const path = require("node:path");
const grpc = require("@grpc/grpc-js");
const protoLoader = require("@grpc/proto-loader");
const fs = require("node:fs");

/// In a checkout this is `contracts/proto/runtime.proto` itself -- the same
/// file the Rust runtime compiles, which is what makes "protocol neutral" a
/// fact rather than a claim.
///
/// A packaged app cannot reach outside its bundle, so the build copies that
/// file in beside the runtime binary. The claim there is weaker and worth
/// stating: it is a copy taken at build time, from the same source, alongside
/// the binary built from the same tree.
const BUNDLED = process.resourcesPath
  ? path.join(process.resourcesPath, "runtime.proto")
  : null;
const PROTO = BUNDLED && fs.existsSync(BUNDLED)
  ? BUNDLED
  : path.join(__dirname, "..", "..", "..", "contracts", "proto", "runtime.proto");

let client = null;
let describe = "not connected";

function load() {
  const definition = protoLoader.loadSync(PROTO, {
    keepCase: true,
    longs: String,
    enums: String,
    defaults: true,
    oneofs: true,
  });
  return grpc.loadPackageDefinition(definition).agent.runtime.v1;
}

/// Opens the connection.
///
/// Plaintext is permitted only for a loopback address. Anything else must
/// present mTLS: the runtime refuses to serve a network surface without it, and
/// a client that would happily talk plaintext to a remote host is a client that
/// will eventually be pointed at one.
function connect({ endpoint, credentials }) {
  const loopback = /^(127\.0\.0\.1|\[::1\]|localhost):\d+$/.test(endpoint);
  if (!loopback && !credentials) {
    throw new Error("a non-loopback runtime requires mTLS credentials");
  }
  const v1 = load();
  const channel = loopback && !credentials
    ? grpc.credentials.createInsecure()
    : grpc.credentials.createSsl(credentials.ca, credentials.key, credentials.cert);
  client = new v1.RuntimeInvocation(endpoint, channel);
  describe = endpoint;
  return { endpoint, secure: !loopback || Boolean(credentials) };
}

function status() {
  return { connected: Boolean(client), endpoint: client ? describe : null };
}

/// Every call carries the operator bearer token. The runtime takes tenant,
/// application and workload identity from the verified claims — the request
/// body may only agree with them — so this metadata is the whole of the
/// client's authority.
function auth(token) {
  const meta = new grpc.Metadata();
  if (token) meta.set("authorization", `Bearer ${token}`);
  return meta;
}

function call(method, request, token) {
  return new Promise((resolve, reject) => {
    if (!client) return reject(new Error("not connected to a runtime"));
    client[method](request, auth(token), (error, response) =>
      error ? reject(error) : resolve(response),
    );
  });
}

const readEvents = (request, token) => call("ReadEvents", request, token);
const submit = (request, token) => call("Submit", request, token);
const control = (request, token) => call("Control", request, token);

/// Follows a run. The exclusive cursor means a dropped stream is resumed by
/// reconnecting at the last sequence seen, so `onItem` may be called again
/// after a gap without the caller having to de-duplicate.
function watchEvents(request, token, onItem, onEnd) {
  if (!client) throw new Error("not connected to a runtime");
  const stream = client.WatchEvents(request, auth(token));
  stream.on("data", onItem);
  stream.on("end", () => onEnd(null));
  stream.on("error", (error) => onEnd(error));
  return () => stream.cancel();
}

module.exports = { connect, status, readEvents, submit, control, watchEvents, PROTO };
