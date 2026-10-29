#!/usr/bin/env node
// Publish one Stampede release: upload the APK to Blossom, verify the
// mirror the way a DEVICE will use it, then announce a signed kind-30063
// event on the release relays. Ported from Skative's publish-release.mjs.
//
//   node tools/release/publish-release.mjs <apk> --version 0.2.0 --version-code 2000 \
//     [--notes "..."] [--mirror https://extra.mirror] [--relay wss://extra.relay] [--dry-run] [--allow-cert-change]
//   node tools/release/publish-release.mjs --init-key       # mint the release key once
//   node tools/release/publish-release.mjs --print-pubkey   # print the release key's PUBLIC half only (hex and npub)
//   node tools/release/publish-release.mjs --self-test      # sign + verify a sample, no files, no network
//
// What it checks before it signs anything:
//  * the release key (STAMPEDE_RELEASE_KEY_FILE, default: the Stampede release key
//    file, .stampede-release/release-key.hex in the home directory) is the one the app
//    pins (RELEASE_PUBKEY_HEX in app/src/update.rs). While the app pins NO key
//    (RELEASE_PUBKEY_HEX = None) a real publish is refused: mint the key with
//    --init-key and pin its public half first;
//  * --version-code and --version equal the APK's own versionCode / versionName
//    (`aapt2 dump badging`) and its package is org.decented.stampede: the
//    app only offers an update whose version_code is higher than the installed one;
//  * the signing certificate sha256 (the `cert` tag) comes from the APK itself
//    via `apksigner verify --print-certs`, never from the command line, and must be
//    the cert every release so far carried unless --allow-cert-change is passed.
//
// Exit non-zero when: no Blossom mirror served the bytes back at their
// canonical address with a direct 200 and a matching sha256, or every relay
// refused the event. --dry-run signs and prints the event but never uploads
// or publishes (no network at all). It signs with an EPHEMERAL throwaway key,
// never the release key (the file is not even read): the printed event is a
// valid-looking announcement that no device trusts, so pasting it anywhere
// cannot displace a real release.
//
// Env: STAMPEDE_RELEASE_KEY_FILE, STAMPEDE_RELEASE_RELAYS (comma-separated extra relays, like --relay), ANDROID_HOME (default: Android/Sdk in the home directory),
//      STAMPEDE_APKSIGNER / STAMPEDE_AAPT2 (default: newest build-tools).
import { finalizeEvent, generateSecretKey, getPublicKey, verifyEvent } from 'nostr-tools/pure';
import { nip19 } from 'nostr-tools';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { existsSync, chmodSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
	DEFAULT_BLOSSOM_MIRRORS,
	RELEASE_CHANNEL,
	resolveReleaseRelays,
	buildBlossomAuth,
	EXPECTED_CERT_SHA256,
	buildReleaseEvent,
	checkSigningCert,
	pinnedPubkeyFrom,
	isCanonicalBlossomUrl
} from './release-helpers.mjs';

const APP_ID = 'org.decented.stampede';
const HERE = dirname(fileURLToPath(import.meta.url));
const UPDATE_RS = join(HERE, '..', '..', 'app', 'src', 'update.rs');
const KEY_FILE = process.env.STAMPEDE_RELEASE_KEY_FILE ?? join(homedir(), '.stampede-release', 'release-key.hex');

/** Newest build-tools directory under the Android SDK (what tools/build-apk.sh uses). */
function buildTool(envName, tool) {
	if (process.env[envName]) return process.env[envName];
	const sdk = process.env.ANDROID_HOME ?? join(homedir(), 'Android', 'Sdk');
	const dir = join(sdk, 'build-tools');
	const versions = existsSync(dir)
		? readdirSync(dir).sort((a, b) => a.localeCompare(b, undefined, { numeric: true }))
		: [];
	if (versions.length === 0) throw new Error(`no Android build-tools under ${dir} (set ANDROID_HOME or ${envName})`);
	return join(dir, versions[versions.length - 1], tool);
}

/** The value after a flag; a missing one is an error, not `undefined`. */
function needValue(argv, i, flag) {
	if (i >= argv.length) throw new Error(`${flag} needs a value`);
	return argv[i];
}

function parseArgs(argv) {
	const args = { notes: '', mirrors: [], relays: [] };
	for (let i = 0; i < argv.length; i++) {
		const a = argv[i];
		if (a === '--dry-run') args.dryRun = true;
		else if (a === '--allow-cert-change') args.allowCertChange = true;
		else if (a === '--self-test') args.selfTest = true;
		else if (a === '--init-key') args.initKey = true;
		else if (a === '--print-pubkey') args.printPubkey = true;
		else if (a === '--version') args.version = needValue(argv, ++i, a);
		else if (a === '--version-code') args.versionCode = Number(needValue(argv, ++i, a));
		else if (a === '--notes') args.notes = needValue(argv, ++i, a);
		else if (a === '--mirror') args.mirrors.push(needValue(argv, ++i, a));
		else if (a === '--relay') args.relays.push(needValue(argv, ++i, a));
		else if (a.startsWith('-')) throw new Error(`unknown argument: ${a}`);
		else if (args.apkPath === undefined) args.apkPath = a;
		else throw new Error(`unexpected extra positional argument: ${a}`);
	}
	return args;
}

/**
 * Tiny hermetic self-test: builds a sample release event with an ephemeral
 * throwaway key, signs it, and checks nostr-tools' own verifyEvent accepts
 * it. Touches no files, no network, no real release key.
 */
function runSelfTest() {
	const sk = generateSecretKey();
	const sha256 = 'a'.repeat(64);
	const event = finalizeEvent(
		buildReleaseEvent({
			channel: RELEASE_CHANNEL,
			versionName: '0.0.1',
			versionCode: 1,
			sha256,
			sizeBytes: 1234,
			urls: [`https://nostr.download/${sha256}.apk`],
			certSha256: 'b'.repeat(64),
			notes: 'self-test',
			createdAt: Math.floor(Date.now() / 1000)
		}),
		sk
	);
	const ok = verifyEvent(event);
	console.log(`self-test: built + signed a sample kind-${event.kind} event; verifyEvent = ${ok}`);
	if (!ok) {
		console.error('self-test FAILED: signed event did not verify');
		process.exit(1);
	}
	console.log('self-test PASSED');
}

/** Mint the release key once. Prints the public half only; the secret stays in the file. */
function initKey() {
	if (existsSync(KEY_FILE)) {
		console.log(`release key already exists at ${KEY_FILE}`);
		printPubkey(getPublicKey(readKey()));
		return;
	}
	mkdirSync(dirname(KEY_FILE), { recursive: true, mode: 0o700 });
	chmodSync(dirname(KEY_FILE), 0o700); // mkdir's mode does not tighten a directory that already exists
	const sk = generateSecretKey();
	// 'wx': never overwrite a key another run created in the meantime.
	writeFileSync(KEY_FILE, Buffer.from(sk).toString('hex') + '\n', { mode: 0o600, flag: 'wx' });
	chmodSync(KEY_FILE, 0o600);
	console.log(`generated a new release key at ${KEY_FILE} (mode 0600; the secret is never printed)`);
	printPubkey(getPublicKey(sk));
}

function printPubkey(pk) {
	console.log(`  release pubkey (hex)  = ${pk}`);
	console.log(`  release pubkey (npub) = ${nip19.npubEncode(pk)}`);
	console.log('Pin the hex value in app/src/update.rs: RELEASE_PUBKEY_HEX: Option<&str> = Some("<hex>");');
}

function readKey() {
	if (!existsSync(KEY_FILE)) throw new Error(`no release key at ${KEY_FILE}; run: node tools/release/publish-release.mjs --init-key`);
	const hex = readFileSync(KEY_FILE, 'utf8').trim();
	if (!/^[0-9a-f]{64}$/.test(hex)) throw new Error(`${KEY_FILE} is not a 64-hex secret`);
	return Uint8Array.from(Buffer.from(hex, 'hex'));
}

/**
 * The pubkey the app pins, read from app/src/update.rs: a hex string, 'none'
 * when the constant is `None` (no release key pinned yet), or null when the
 * constant cannot be found.
 */
function pinnedPubkey() {
	try {
		return pinnedPubkeyFrom(readFileSync(UPDATE_RS, 'utf8'));
	} catch {
		return null;
	}
}

/**
 * The signing certificate's sha256, read from the APK itself via
 * `apksigner verify --print-certs`, so the announced cert can never drift from
 * what is actually on the artifact being announced.
 */
function certSha256FromApk(apkPath) {
	let out;
	try {
		out = execFileSync(buildTool('STAMPEDE_APKSIGNER', 'apksigner'), ['verify', '--print-certs', apkPath], {
			encoding: 'utf8',
			stdio: ['ignore', 'pipe', 'pipe']
		});
	} catch (err) {
		throw new Error((err.stderr || err.message || '').toString().trim().split('\n')[0] || String(err));
	}
	const m = /Signer #1 certificate SHA-256 digest:\s*([0-9a-fA-F]{64})/.exec(out);
	if (!m) throw new Error(`could not parse a SHA-256 cert digest from apksigner output:\n${out}`);
	return m[1].toLowerCase();
}

/** The APK's own package, versionCode and versionName (`aapt2 dump badging`). */
function badgingOf(apkPath) {
	const out = execFileSync(buildTool('STAMPEDE_AAPT2', 'aapt2'), ['dump', 'badging', apkPath], {
		encoding: 'utf8',
		stdio: ['ignore', 'pipe', 'pipe']
	});
	const m = /^package: name='([^']*)' versionCode='(\d+)' versionName='([^']*)'/m.exec(out);
	if (!m) throw new Error(`could not read the package line from aapt2 output:\n${out.slice(0, 400)}`);
	return { name: m[1], versionCode: Number(m[2]), versionName: m[3] };
}

async function uploadToBlossom(server, bytes, sha256, sk) {
	const auth = finalizeEvent(buildBlossomAuth({ sha256, createdAt: Math.floor(Date.now() / 1000) }), sk);
	const header = `Nostr ${Buffer.from(JSON.stringify(auth)).toString('base64')}`;
	const res = await fetch(`${server}/upload`, {
		method: 'PUT',
		headers: {
			Authorization: header,
			'Content-Type': 'application/octet-stream',
			'X-SHA-256': sha256
		},
		body: bytes
	});
	if (!res.ok) throw new Error(`${server}: upload HTTP ${res.status}`);
}

async function fetchDirect(url) {
	// redirect: 'manual' - a mirror is announced only if it serves the bytes
	// directly at the canonical address. (A device would follow a few https
	// redirects, hash-checked, but nothing relies on that: a redirect target
	// is an implementation detail that can vanish.)
	const res = await fetch(url, { redirect: 'manual' });
	if (res.status === 200) return { bytes: Buffer.from(await res.arrayBuffer()) };
	if ([301, 302, 307, 308].includes(res.status)) return { location: res.headers.get('location') };
	return { error: `HTTP ${res.status}` };
}

/**
 * Verify a mirror the way a DEVICE will use it: GET, no redirects, full-body
 * sha256. The event may only ever carry the blob's CANONICAL root address
 * (isCanonicalBlossomUrl), never a redirect target. A redirecting server
 * still counts as an upload success (the blob is there), but it contributes
 * no URL: "blob present, not device-servable."
 */
async function verifyBlossom(server, sha256) {
	for (const url of [`${server}/${sha256}.apk`, `${server}/${sha256}`]) {
		const r = await fetchDirect(url);
		if (r.bytes) {
			const got = createHash('sha256').update(r.bytes).digest('hex');
			if (got !== sha256) {
				console.error(`mirror ${url}: serves WRONG bytes (${got})`);
				return null;
			}
			return url;
		}
		if (r.location) {
			console.error(`mirror ${url}: blob present, not device-servable (redirects to ${r.location})`);
			continue;
		}
		console.error(`mirror ${url}: ${r.error}`);
	}
	return null;
}

function publishToRelay(url, event, timeoutMs = 10_000) {
	return new Promise((resolve) => {
		let settled = false;
		let ws;
		const done = (ok, reason) => {
			if (settled) return;
			settled = true;
			try {
				ws?.close();
			} catch {
				/* already closed */
			}
			resolve({ url, ok, reason });
		};
		try {
			ws = new WebSocket(url);
		} catch (err) {
			done(false, err.message);
			return;
		}
		const timer = setTimeout(() => done(false, 'timeout'), timeoutMs);
		ws.onopen = () => ws.send(JSON.stringify(['EVENT', event]));
		ws.onmessage = (m) => {
			try {
				const [type, id, ok, reason] = JSON.parse(m.data);
				if (type === 'OK' && id === event.id) {
					clearTimeout(timer);
					done(Boolean(ok), reason ?? '');
				}
			} catch {
				/* ignore non-JSON frames */
			}
		};
		ws.onerror = () => {
			clearTimeout(timer);
			done(false, 'socket error');
		};
		ws.onclose = () => {
			clearTimeout(timer);
			done(false, 'closed-before-ok');
		};
	});
}

async function main() {
	const args = parseArgs(process.argv.slice(2));

	if (args.initKey) {
		initKey();
		return;
	}
	if (args.printPubkey) {
		// The public half only: the secret is read to derive it and never printed or logged.
		const pk = getPublicKey(readKey());
		console.log(`release pubkey (hex)  = ${pk}`);
		console.log(`release pubkey (npub) = ${nip19.npubEncode(pk)}`);
		return;
	}
	if (args.selfTest && args.apkPath === undefined) {
		runSelfTest();
		return;
	}

	if (args.apkPath === undefined)
		throw new Error('usage: publish-release.mjs <apk> --version <semver> --version-code <int> [--notes "..."] [--mirror <url>] [--relay <wss-url>] [--dry-run] [--allow-cert-change]  |  --init-key  |  --print-pubkey  |  --self-test');
	if (typeof args.version !== 'string' || args.version.length === 0) throw new Error('--version is required');
	if (!Number.isInteger(args.versionCode) || args.versionCode <= 0)
		throw new Error('--version-code is required and must be a positive integer');
	if (!existsSync(args.apkPath)) throw new Error(`no such file: ${args.apkPath}`);
	// Validated before anything is signed or uploaded.
	const relays = resolveReleaseRelays(args.relays, process.env.STAMPEDE_RELEASE_RELAYS);
	console.log(`relays: ${relays.join(' ')}`);

	// The APK must be what the flags say: the app compares the announced
	// version_code with the installed one, so a mismatch would strand or loop users.
	const badging = badgingOf(args.apkPath);
	if (badging.name !== APP_ID) throw new Error(`the APK's package is ${badging.name}, not ${APP_ID}`);
	if (badging.versionCode !== args.versionCode)
		throw new Error(`--version-code ${args.versionCode} but the APK's versionCode is ${badging.versionCode}`);
	if (badging.versionName !== args.version)
		throw new Error(`--version ${args.version} but the APK's versionName is ${badging.versionName}`);
	console.log(`apk: ${badging.name} ${badging.versionName} (versionCode ${badging.versionCode})`);

	let sk;
	if (args.dryRun) {
		// Never the release key: a dry run's output gets pasted into tickets and fixtures,
		// and an event signed by the real key is a publishable release announcement.
		sk = generateSecretKey();
		console.log(`DRY RUN: signing with a throwaway key ${getPublicKey(sk)}, NOT the release key; the app would reject this event`);
	} else {
		sk = readKey();
		const pk = getPublicKey(sk);
		const pinned = pinnedPubkey();
		if (pinned === null) throw new Error(`could not find RELEASE_PUBKEY_HEX in ${UPDATE_RS}`);
		if (pinned === 'none')
			throw new Error("the app pins no release key yet (RELEASE_PUBKEY_HEX is None in app/src/update.rs); pin this key's public half and rebuild before publishing");
		if (pinned !== pk) throw new Error(`the release key (${pk}) is not the one the app pins (${pinned}); refusing to sign`);
		console.log(`release key: ${pk} (matches the app's pinned key)`);
	}

	const bytes = readFileSync(args.apkPath);
	const sha256 = createHash('sha256').update(bytes).digest('hex');
	const sizeBytes = bytes.length; // the same bytes the hash covers
	console.log(`${args.apkPath}: sha256=${sha256} size=${sizeBytes}`);

	const cert = certSha256FromApk(args.apkPath);
	console.log(`signing certificate sha256: ${cert}`);
	if (checkSigningCert(cert, args.allowCertChange === true) === 'changed')
		console.warn(`WARNING: --allow-cert-change: the cert differs from ${EXPECTED_CERT_SHA256}; installed copies will not be able to update to this APK`);

	const servers = [...new Set([...DEFAULT_BLOSSOM_MIRRORS, ...args.mirrors])].map((s) => s.replace(/\/$/, ''));
	// Before anything is uploaded (the BUD-02 auth must not cross in clear, and one bad
	// mirror must not get a release signed that every device then drops): each mirror must
	// be a bare https origin whose blob address the app accepts, byte for byte.
	for (const server of servers) {
		if (!isCanonicalBlossomUrl(`${server}/${sha256}`, sha256))
			throw new Error(`bad --mirror ${JSON.stringify(server)}: it must be https://host[:port] with no path, and a plain DNS host name`);
	}

	const urls = [];
	if (args.dryRun) {
		// No upload, no network: derive the canonical addresses the mirrors would serve.
		for (const s of servers) urls.push(`${s}/${sha256}.apk`);
	} else {
		let uploaded = 0;
		for (const server of servers) {
			try {
				await uploadToBlossom(server, bytes, sha256, sk);
				uploaded++;
				const verified = await verifyBlossom(server, sha256);
				if (verified) {
					urls.push(verified);
					console.log(`mirror ok: ${verified}`);
				} else {
					console.error(`mirror ${server}: uploaded, but no device-servable URL`);
				}
			} catch (err) {
				console.error(`mirror FAILED: ${err.message}`);
			}
		}
		if (uploaded === 0) {
			console.error('no Blossom mirror accepted the upload; refusing to announce');
			process.exit(1);
		}
	}

	if (urls.length === 0) {
		console.error('no Blossom mirror serves these bytes at a canonical address; refusing to announce a release nothing can download');
		process.exit(1);
	}
	const bad = urls.filter((u) => !isCanonicalBlossomUrl(u, sha256));
	if (bad.length) {
		console.error(`refusing to announce non-canonical mirror url(s): ${bad.join(', ')}`);
		process.exit(1);
	}

	const event = finalizeEvent(
		buildReleaseEvent({
			channel: RELEASE_CHANNEL,
			versionName: args.version,
			versionCode: args.versionCode,
			sha256,
			sizeBytes,
			urls: [...new Set(urls)],
			certSha256: cert,
			notes: args.notes,
			createdAt: Math.floor(Date.now() / 1000)
		}),
		sk
	);
	if (!verifyEvent(event)) throw new Error('internal error: the signed event does not verify');

	if (args.dryRun) {
		console.log('--dry-run: signed event follows; nothing uploaded or published');
		console.log(JSON.stringify(event, null, 2));
		return;
	}

	const results = await Promise.all(relays.map((r) => publishToRelay(r, event)));
	for (const r of results) {
		console.log(`${r.ok ? 'relay ok  ' : 'relay FAIL'}: ${r.url} ${r.reason ?? ''}`);
	}
	if (!results.some((r) => r.ok)) {
		console.error('every relay refused the release event');
		process.exit(1);
	}
	console.log(`announced ${RELEASE_CHANNEL} ${args.version} (code ${args.versionCode})`);
}

main().catch((err) => {
	console.error(err.message);
	process.exit(1);
});
