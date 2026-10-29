// Pure builders for the Stampede release pipeline: no network, no
// filesystem, so `node --test` covers them exactly. Ported from Skative's
// tools/release/release-helpers.mjs (itself from Vitark Train and Fathom).
//
// The app's compiled-in trust anchor (RELEASE_PUBKEY_HEX in app/src/update.rs)
// is the source of truth for which pubkey is trusted; publish-release.mjs
// checks the key it loads against that constant before it signs anything.
export const SOFTWARE_RELEASE_KIND = 30063;
export const RELEASE_RELAYS = ['wss://relay.trotters.cc', 'wss://relay.damus.io', 'wss://nos.lol'];
export const RELEASE_CHANNEL = 'stampede-android-apk';
/** BUD-02 Blossom upload authorization kind. */
export const BLOSSOM_AUTH_KIND = 24242;
/**
 * Default Blossom mirror set; --mirror on publish-release.mjs adds more.
 *
 * blossom.primal.net is not in it: probed on 2026-09-08 with slices of a
 * real APK signed by the release key, it takes 3 MB and answers
 * "blob import error" (HTTP 500) at 4 MB — a per-account quota for a key
 * that is not a Primal user, not a transport limit. An APK is 66 MB.
 */
export const DEFAULT_BLOSSOM_MIRRORS = ['https://nostr.download'];

const U32_MAX = 4294967295;
/** The largest APK an announcement may name: the same cap as `MAX_APK_BYTES` in app/src/update.rs. */
export const MAX_APK_BYTES = 256 * 1024 * 1024;
const HEX64 = /^[0-9a-f]{64}$/;
const DECIMAL = /^[0-9]+$/;
/** https://host[:port]/<sha>[.ext], matched against the raw string (see isCanonicalBlossomUrl). */
const CANONICAL_URL = /^https:\/\/[A-Za-z0-9.-]+(?::[0-9]{1,5})?\/([0-9a-f]{64})(?:\.[A-Za-z0-9]{1,8})?$/;

/**
 * The signing certificate every Stampede release has carried so far. A different
 * cert makes Android refuse the update over the installed app (signature mismatch), so
 * the publisher refuses to announce one unless told to with --allow-cert-change.
 */
export const EXPECTED_CERT_SHA256 = 'eacd24505a50630e2a115226fe592518bc16cc2421cfc019372b58baeaf08aa9';

/**
 * Guard for the APK's signing cert (the `cert` tag). Returns normally when the cert is
 * the expected one, or differs and `allowCertChange` is true (returns 'changed' so the
 * caller can warn); throws otherwise.
 */
export function checkSigningCert(certSha256, allowCertChange = false, expected = EXPECTED_CERT_SHA256) {
	const got = typeof certSha256 === 'string' ? certSha256.toLowerCase() : certSha256;
	if (got === expected) return 'ok';
	if (allowCertChange === true) return 'changed';
	throw new Error(
		`the APK's signing certificate sha256 is ${certSha256}, not the expected ${expected}; ` +
			'installed copies cannot update to an APK signed with a different key. ' +
			'Refusing to publish (pass --allow-cert-change if the change is deliberate)'
	);
}

/**
 * A relay address for --relay / STAMPEDE_RELEASE_RELAYS: `wss://host[:port][/path]` or a
 * bare host (wss:// is added). Returns it normalised (lower-case host, no default
 * port, no trailing slash); throws on anything else (plain ws://, spaces, no dot).
 */
export function parseRelayUrl(input) {
	const bad = () => new Error(`bad relay ${JSON.stringify(input)}: use wss://host[:port][/path] or a bare host`);
	const t = String(input ?? '').trim();
	if (t === '' || /[\s\x00-\x1f]/.test(t)) throw bad();
	const m = /^(?:([A-Za-z][A-Za-z0-9+.-]*):\/\/)?([^/?#\\]*)((?:\/[^?#\\]*)?)$/.exec(t);
	if (!m) throw bad();
	const [, scheme, authority, path] = m;
	if (scheme !== undefined && scheme.toLowerCase() !== 'wss') throw bad();
	const hp = /^([A-Za-z0-9.-]+)(?::([0-9]{1,5}))?$/.exec(authority);
	if (!hp) throw bad();
	const host = hp[1].toLowerCase();
	const labelOk = (l) => l !== '' && !l.startsWith('-') && !l.endsWith('-');
	if (!host.split('.').every(labelOk) || !(host.includes('.') || host === 'localhost')) throw bad();
	let port = hp[2];
	if (port !== undefined) {
		const n = Number(port);
		if (n < 1 || n > 65535) throw bad();
		if (n === 443) port = undefined;
	}
	return `wss://${host}${port ? `:${Number(port)}` : ''}${path.replace(/\/+$/, '')}`;
}

/**
 * The relays a release is announced on: RELEASE_RELAYS (always), then the extra ones
 * from --relay and the comma-separated STAMPEDE_RELEASE_RELAYS, validated and deduped.
 */
export function resolveReleaseRelays(extra = [], envValue = '') {
	const fromEnv = String(envValue ?? '')
		.split(',')
		.map((s) => s.trim())
		.filter((s) => s !== '');
	const out = [];
	const seen = new Set();
	for (const u of [...RELEASE_RELAYS, ...fromEnv, ...extra]) {
		const n = parseRelayUrl(u);
		if (!seen.has(n)) {
			seen.add(n);
			out.push(n);
		}
	}
	return out;
}

/**
 * The release key the app pins, read out of the source of app/src/update.rs:
 * `RELEASE_PUBKEY_HEX: Option<&str> = Some("<64 hex>")` gives the hex, `= None`
 * gives 'none' (no key pinned yet: nothing may be published), anything else null.
 */
export function pinnedPubkeyFrom(source) {
	const m = /RELEASE_PUBKEY_HEX:\s*Option<&str>\s*=\s*(?:None|Some\("([0-9a-f]{64})"\))\s*;/.exec(String(source));
	if (!m) return null;
	return m[1] ?? 'none';
}

/** True for a 64-lowercase-hex string — the shape shared by `x` and `cert`. */
function isHex64(value) {
	return typeof value === 'string' && HEX64.test(value);
}

/** True for an already-typed positive integer (buildReleaseEvent's inputs). */
function isPositiveInteger(n) {
	return Number.isInteger(n) && n > 0;
}

/**
 * True for a tag value shaped like a positive integer: plain decimal digits
 * only — no sign, no leading `+`, no `1.5`/`1e2` — and safely representable
 * as a Number. Mirrors `parse_release` in app/src/update.rs, which takes
 * `version_code` (a u32) and `size` (a u64) as plain decimal digits;
 * summarizeReleaseEvent must reject anything that would make the app's gate
 * reject the event. `max` is the app's type limit (u32 for version_code).
 */
function isPositiveIntegerTag(raw, max = Number.MAX_SAFE_INTEGER) {
	return (
		typeof raw === 'string' &&
		raw.length <= 18 &&
		DECIMAL.test(raw) &&
		Number.isSafeInteger(Number(raw)) &&
		Number(raw) > 0 &&
		Number(raw) <= max
	);
}

/**
 * Does a raw Nostr relay frame TYPE ("EVENT" | "EOSE" | "CLOSED" | "NOTICE" |
 * …) count as the relay having ANSWERED a REQ — used by a release checker's
 * per-relay "answered" tracking. EVENT and EOSE do: the relay actually
 * responded to the query (some relays never send EOSE for a REQ they
 * immediately satisfy with EVENTs, so EVENT alone must count too). CLOSED
 * does NOT: it is how a relay reports a REFUSAL of the subscription itself
 * (`auth-required:`, `error:`, `rate-limited:`, …), not an answer to it.
 * a release checker's exit-3 "no relay answered" gate depends on this being
 * false for CLOSED — three relays all refusing the REQ must read as
 * "couldn't determine the latest release", never as "none exists".
 */
export function isAnsweringFrameType(type) {
	return type === 'EVENT' || type === 'EOSE';
}

/**
 * A URL a device may be told to fetch `sha256` from: https, and the blob at
 * the server ROOT, addressed by its own hash (`https://host/<sha>` or
 * `https://host/<sha>.<ext>`, BUD-01). That path is the one address a
 * Blossom server promises to keep serving. Anything else — in particular a
 * CDN redirect *target* — is an implementation detail that can vanish (see
 * the Vitark/Fathom incident this rule was hard-learned from, documented in
 * their release-helpers.mjs / docs).
 */
export function isCanonicalBlossomUrl(url, sha256) {
	if (typeof url !== 'string' || !HEX64.test(sha256 ?? '')) return false;
	// The RAW string, not a WHATWG-normalised URL: the app (is_canonical_blossom_url
	// in app/src/update.rs) matches the raw tag, and normalisation would accept
	// things it rejects (an IPv6 literal, `_` or IDN hosts, `\` as a separator,
	// stripped tabs and newlines). A mirror this passes, every device accepts.
	const m = CANONICAL_URL.exec(url);
	return m !== null && m[1] === sha256;
}

/**
 * The app's `version` rule (parse_release): 1-32 bytes, no control characters.
 * Same bytes, same Unicode "Cc" class as Rust's `char::is_control`.
 */
export function isValidVersionName(v) {
	return typeof v === 'string' && v.length > 0 && Buffer.byteLength(v, 'utf8') <= 32 && !/\p{Cc}/u.test(v);
}

/**
 * The unsigned kind-30063 template announcing one artifact. Throws on any
 * invalid field — a malformed release event must never reach finalizeEvent.
 * Stampede has one release channel (`stampede-android-apk`) and the cert is
 * always required — there is no artifact type here without a signing
 * certificate.
 */
export function buildReleaseEvent({
	channel,
	versionName,
	versionCode,
	sha256,
	sizeBytes,
	urls,
	certSha256,
	notes,
	createdAt
}) {
	if (channel !== RELEASE_CHANNEL) throw new Error(`bad channel: ${channel}`);
	if (!isValidVersionName(versionName))
		throw new Error('versionName must be 1-32 bytes with no control characters');
	if (!isPositiveInteger(versionCode) || versionCode > U32_MAX)
		throw new Error('versionCode must be a positive integer that fits in 32 bits');
	if (!isHex64(sha256)) throw new Error('sha256 must be 64 lowercase hex chars');
	if (!isPositiveInteger(sizeBytes) || sizeBytes > MAX_APK_BYTES)
		throw new Error(`sizeBytes must be a positive integer of at most ${MAX_APK_BYTES} (the app's cap)`);
	if (!Array.isArray(urls) || urls.length === 0) throw new Error('at least one url required');
	for (const u of urls) {
		if (typeof u !== 'string' || !u.startsWith('https://'))
			throw new Error(`mirror url must be https: ${u}`);
		if (!isCanonicalBlossomUrl(u, sha256))
			throw new Error(
				`mirror url must be the blob's canonical root address (https://host/<sha>[.ext]): ${u}`
			);
	}
	if (!isHex64(certSha256)) throw new Error('certSha256 required (64 lowercase hex chars)');
	if (!Number.isInteger(createdAt) || createdAt <= 0) throw new Error('createdAt required');

	const tags = [
		['d', channel],
		['version', versionName],
		['version_code', String(versionCode)],
		['x', sha256],
		['size', String(sizeBytes)],
		['cert', certSha256]
	];
	for (const u of urls) tags.push(['url', u]);
	return {
		kind: SOFTWARE_RELEASE_KIND,
		created_at: createdAt,
		tags,
		content: notes ?? ''
	};
}

/**
 * The unsigned BUD-02 upload-authorization template. Signed with the release
 * key and sent as `Authorization: Nostr <base64(signed json)>` on the PUT.
 */
export function buildBlossomAuth({ sha256, createdAt }) {
	if (!HEX64.test(sha256)) throw new Error('sha256 must be 64 lowercase hex chars');
	if (!Number.isInteger(createdAt) || createdAt <= 0) throw new Error('createdAt required');
	return {
		kind: BLOSSOM_AUTH_KIND,
		created_at: createdAt,
		tags: [
			['t', 'upload'],
			['x', sha256],
			['expiration', String(createdAt + 600)]
		],
		content: 'Stampede release artifact upload'
	};
}

/**
 * Parse a verified release event's tags into a plain summary object, used by
 * release checkers. Returns null if the event doesn't look like a
 * well-formed release on RELEASE_CHANNEL (caller is expected to have already
 * run verifyEvent + pubkey checks).
 *
 * Enforces exactly the same tag shapes as the app's own gate,
 * `parse_release` in app/src/update.rs: a non-empty `version`, a positive-integer
 * `version_code`, a 64-lowercase-hex `x` (sha256), a positive-integer
 * `size`, a 64-lowercase-hex `cert`, and at least one `url` tag that all
 * pass `isCanonicalBlossomUrl` against that `x`. A summary this function
 * returns non-null for must be an event the app would also accept — this is
 * the operator-facing "did the publish land correctly" check, so it must
 * never be looser than the device's own verification.
 */
export function summarizeReleaseEvent(ev) {
	if (typeof ev !== 'object' || ev === null) return null;
	const tags = Array.isArray(ev.tags) ? ev.tags : [];
	const get = (name) => tags.find((t) => t[0] === name)?.[1];

	const d = get('d');
	if (d !== RELEASE_CHANNEL) return null;

	const version = get('version');
	if (!isValidVersionName(version)) return null;

	const versionCodeRaw = get('version_code');
	if (!isPositiveIntegerTag(versionCodeRaw, U32_MAX)) return null;
	const versionCode = Number(versionCodeRaw);

	const sha256 = get('x');
	if (!isHex64(sha256)) return null;

	const sizeRaw = get('size');
	if (!isPositiveIntegerTag(sizeRaw, MAX_APK_BYTES)) return null;
	const sizeBytes = Number(sizeRaw);

	const certSha256 = get('cert');
	if (!isHex64(certSha256)) return null;

	const urls = tags.filter((t) => t[0] === 'url').map((t) => t[1]);
	if (urls.length === 0 || urls.some((u) => !isCanonicalBlossomUrl(u, sha256))) return null;

	return {
		id: ev.id,
		pubkey: ev.pubkey,
		createdAt: ev.created_at,
		channel: d,
		version,
		versionCode,
		sha256,
		sizeBytes,
		certSha256,
		urls,
		notes: ev.content ?? ''
	};
}
