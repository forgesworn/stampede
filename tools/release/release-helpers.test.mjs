import { test } from 'node:test';
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import {
  RELEASE_CHANNEL,
  SOFTWARE_RELEASE_KIND,
  buildBlossomAuth,
  EXPECTED_CERT_SHA256,
  buildReleaseEvent,
  checkSigningCert,
  isAnsweringFrameType,
  isCanonicalBlossomUrl,
  isValidVersionName,
  MAX_APK_BYTES,
  parseRelayUrl,
  pinnedPubkeyFrom,
  RELEASE_RELAYS,
  resolveReleaseRelays,
  summarizeReleaseEvent
} from './release-helpers.mjs';

const SHA = 'a'.repeat(64);
const CERT = 'b'.repeat(64);
const ok = {
  channel: RELEASE_CHANNEL,
  versionName: '0.1.0',
  versionCode: 1,
  sha256: SHA,
  sizeBytes: 27693181,
  urls: [`https://blossom.primal.net/${SHA}.apk`],
  certSha256: CERT,
  notes: 'notes',
  createdAt: 1757000000
};

test('buildReleaseEvent emits the tags in the agreed order', () => {
  const ev = buildReleaseEvent(ok);
  assert.equal(ev.kind, SOFTWARE_RELEASE_KIND);
  assert.deepEqual(ev.tags, [
    ['d', 'stampede-android-apk'],
    ['version', '0.1.0'],
    ['version_code', '1'],
    ['x', SHA],
    ['size', '27693181'],
    ['cert', CERT],
    ['url', `https://blossom.primal.net/${SHA}.apk`]
  ]);
  assert.equal(ev.content, 'notes');
});

test('buildReleaseEvent refuses anything malformed, naming the field', () => {
  for (const [bad, want] of [
    [{ channel: 'some-other-apk' }, /bad channel/],
    [{ versionName: '' }, /versionName/],
    [{ versionName: 'x'.repeat(33) }, /versionName/],
    [{ versionName: '1.0\n' }, /versionName/],
    [{ versionCode: 0 }, /versionCode/],
    [{ versionCode: 1.5 }, /versionCode/],
    [{ versionCode: 4294967296 }, /versionCode/],
    [{ sha256: 'short' }, /sha256/],
    [{ sizeBytes: 0 }, /sizeBytes/],
    [{ sizeBytes: MAX_APK_BYTES + 1 }, /sizeBytes/],
    [{ urls: [] }, /at least one url/],
    [{ urls: [`http://h/${SHA}`] }, /must be https/],
    [{ urls: [`https://h/uploads/${SHA}`] }, /canonical root address/],
    [{ urls: [`https://blossom_x.example/${SHA}`] }, /canonical root address/],
    [{ certSha256: 'nope' }, /certSha256/],
    [{ createdAt: 0 }, /createdAt/]
  ]) {
    assert.throws(() => buildReleaseEvent({ ...ok, ...bad }), want, JSON.stringify(bad));
  }
});

// The same list app/src/update.rs reads (is_canonical_blossom_url): one grammar, two implementations.
test('isCanonicalBlossomUrl agrees with the shared vector list the app tests against', () => {
  const vectors = JSON.parse(readFileSync(new URL('./url-vectors.json', import.meta.url), 'utf8'));
  assert.ok(vectors.length > 20);
  for (const { url, ok: want } of vectors) {
    const u = url.replaceAll('<sha>', SHA).replaceAll('<SHAUP>', SHA.toUpperCase());
    assert.equal(isCanonicalBlossomUrl(u, SHA), want, JSON.stringify(u));
  }
});

test('isValidVersionName: 1-32 bytes, no control characters', () => {
  assert.equal(isValidVersionName('0.2.0'), true);
  assert.equal(isValidVersionName('x'.repeat(32)), true);
  assert.equal(isValidVersionName('x'.repeat(33)), false);
  assert.equal(isValidVersionName('\u00e9'.repeat(17)), false, 'bytes, not characters');
  assert.equal(isValidVersionName('1.0\u0007'), false);
  assert.equal(isValidVersionName('1.0\u0085'), false);
  assert.equal(isValidVersionName(''), false);
});

test('isCanonicalBlossomUrl accepts only the blob root address', () => {
  assert.equal(isCanonicalBlossomUrl(`https://h/${SHA}`, SHA), true);
  assert.equal(isCanonicalBlossomUrl(`https://h/${SHA}.apk`, SHA), true);
  assert.equal(isCanonicalBlossomUrl(`https://h/${SHA}?x=1`, SHA), false);
  assert.equal(isCanonicalBlossomUrl(`https://h/a/b/${SHA}`, SHA), false);
  assert.equal(isCanonicalBlossomUrl(`http://h/${SHA}`, SHA), false);
});

test('CLOSED is a refusal, not an answer', () => {
  assert.equal(isAnsweringFrameType('EVENT'), true);
  assert.equal(isAnsweringFrameType('EOSE'), true);
  assert.equal(isAnsweringFrameType('CLOSED'), false);
  assert.equal(isAnsweringFrameType('NOTICE'), false);
});

test('buildBlossomAuth is a BUD-02 upload template that expires', () => {
  const a = buildBlossomAuth({ sha256: SHA, createdAt: 1757000000 });
  assert.equal(a.kind, 24242);
  assert.deepEqual(a.tags, [
    ['t', 'upload'],
    ['x', SHA],
    ['expiration', String(1757000000 + 600)]
  ]);
  assert.throws(() => buildBlossomAuth({ sha256: 'short', createdAt: 1 }));
});

test('summarizeReleaseEvent extracts the fields a release checker prints', () => {
  const ev = { ...buildReleaseEvent(ok), id: 'deadbeef', pubkey: 'f'.repeat(64) };
  const summary = summarizeReleaseEvent(ev);
  assert.equal(summary.version, '0.1.0');
  assert.equal(summary.versionCode, 1);
  assert.equal(summary.sha256, SHA);
  assert.equal(summary.certSha256, CERT);
  assert.deepEqual(summary.urls, [`https://blossom.primal.net/${SHA}.apk`]);
});

test('summarizeReleaseEvent rejects a wrong channel or missing version_code', () => {
  const wrongChannel = { ...buildReleaseEvent(ok) };
  wrongChannel.tags = wrongChannel.tags.map((t) => (t[0] === 'd' ? ['d', 'other-apk'] : t));
  assert.equal(summarizeReleaseEvent(wrongChannel), null);
  assert.equal(summarizeReleaseEvent({ tags: [] }), null);
  assert.equal(summarizeReleaseEvent(null), null);
});

// Replaces the value of the first `name` tag on a freshly-built `ok` event —
// used to probe summarizeReleaseEvent with exactly one malformed field at a
// time, the same way parse_release in app/src/update.rs rejects one bad tag.
function withTag(name, value) {
  const ev = buildReleaseEvent(ok);
  ev.tags = ev.tags.map((t) => (t[0] === name ? [name, value] : t));
  return ev;
}

test('summarizeReleaseEvent rejects everything the app gate (parse_release in update.rs) rejects', () => {
  // sha256 (`x` tag): must be 64 lowercase hex chars, same as buildReleaseEvent's own check.
  assert.equal(summarizeReleaseEvent(withTag('x', 'short')), null);
  assert.equal(summarizeReleaseEvent(withTag('x', 'A'.repeat(64))), null, 'uppercase hex is not lowercase hex');

  // cert: must be 64 lowercase hex chars.
  assert.equal(summarizeReleaseEvent(withTag('cert', 'nope')), null);
  assert.equal(summarizeReleaseEvent(withTag('cert', 'C'.repeat(64))), null, 'uppercase hex is not lowercase hex');

  // size: must be a positive integer, encoded as a plain decimal tag string.
  assert.equal(summarizeReleaseEvent(withTag('size', '0')), null);
  assert.equal(summarizeReleaseEvent(withTag('size', '-1')), null);
  assert.equal(summarizeReleaseEvent(withTag('size', '1.5')), null);
  assert.equal(summarizeReleaseEvent(withTag('size', 'abc')), null);
  // ... and at most the app's cap (MAX_APK_BYTES in app/src/update.rs).
  assert.equal(summarizeReleaseEvent(withTag('size', String(MAX_APK_BYTES + 1))), null);
  assert.equal(summarizeReleaseEvent(withTag('size', String(MAX_APK_BYTES)))?.sizeBytes, MAX_APK_BYTES);

  // version_code: same shape requirement as size.
  assert.equal(summarizeReleaseEvent(withTag('version_code', '0')), null);
  assert.equal(summarizeReleaseEvent(withTag('version_code', '1.5')), null);
  assert.equal(summarizeReleaseEvent(withTag('version_code', '4294967296')), null, 'version_code is a u32 in the app');
  assert.notEqual(summarizeReleaseEvent(withTag('version_code', '4294967295')), null);

  // version: must be present and non-empty, at most 32 bytes, no control characters, as parse_release requires.
  assert.equal(summarizeReleaseEvent(withTag('version', '')), null);
  assert.equal(summarizeReleaseEvent(withTag('version', 'x'.repeat(33))), null);
  assert.equal(summarizeReleaseEvent(withTag('version', '1.0\n')), null);

  // url: every url tag must pass isCanonicalBlossomUrl against the event's own sha256 —
  // a CDN path, not the blob's canonical root address, is rejected.
  const nonCanonical = buildReleaseEvent(ok);
  nonCanonical.tags = nonCanonical.tags.map((t) => (t[0] === 'url' ? ['url', `https://h/uploads/${SHA}`] : t));
  assert.equal(summarizeReleaseEvent(nonCanonical), null);

  const noUrls = buildReleaseEvent(ok);
  noUrls.tags = noUrls.tags.filter((t) => t[0] !== 'url');
  assert.equal(summarizeReleaseEvent(noUrls), null);
});

test('the signing cert guard accepts the known cert and refuses any other', () => {
  assert.equal(EXPECTED_CERT_SHA256, 'eacd24505a50630e2a115226fe592518bc16cc2421cfc019372b58baeaf08aa9');
  assert.equal(checkSigningCert(EXPECTED_CERT_SHA256), 'ok');
  assert.equal(checkSigningCert(EXPECTED_CERT_SHA256.toUpperCase()), 'ok');
  assert.throws(() => checkSigningCert(CERT), /signing certificate.*--allow-cert-change/s);
  assert.throws(() => checkSigningCert(CERT, false), /Refusing to publish/);
  assert.throws(() => checkSigningCert(CERT, 'yes'), /Refusing to publish/);
  assert.throws(() => checkSigningCert(undefined), /Refusing to publish/);
});

test('--allow-cert-change overrides the guard for a changed cert only', () => {
  assert.equal(checkSigningCert(CERT, true), 'changed');
  assert.equal(checkSigningCert(EXPECTED_CERT_SHA256, true), 'ok');
});

test('the publisher CLI knows --allow-cert-change', () => {
  // No APK given: it must reach the usage error, not "unknown argument". Nothing is
  // uploaded or signed on this path.
  const r = spawnSync(process.execPath, [new URL('./publish-release.mjs', import.meta.url).pathname, '--allow-cert-change'], {
    encoding: 'utf8'
  });
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /usage: publish-release\.mjs/);
  assert.doesNotMatch(r.stderr, /unknown argument/);
});

test('relay addresses are validated and normalised', () => {
  assert.equal(parseRelayUrl('wss://Relay.Example.com/'), 'wss://relay.example.com');
  assert.equal(parseRelayUrl('relay.example.com'), 'wss://relay.example.com');
  assert.equal(parseRelayUrl(' relay.example.com:7777/nostr/ '), 'wss://relay.example.com:7777/nostr');
  assert.equal(parseRelayUrl('wss://relay.example.com:443'), 'wss://relay.example.com');
  for (const bad of ['', '  ', 'relay', 'ws://relay.example.com', 'https://relay.example.com', 'wss://', 'a b.com', 'a..b.com', '-a.com', 'a.com:0', 'a.com:70000', 'a.com/x?y=1', 'a.com#f']) {
    assert.throws(() => parseRelayUrl(bad), /bad relay/, JSON.stringify(bad));
  }
});

test('extra relays from --relay and STAMPEDE_RELEASE_RELAYS are added after the default three, deduped', () => {
  assert.deepEqual(resolveReleaseRelays(), RELEASE_RELAYS);
  const got = resolveReleaseRelays(['mine.example.com', 'wss://NOS.LOL/', 'wss://flag.example.org'], 'env1.example.com, wss://mine.example.com/ ,');
  assert.deepEqual(got, [...RELEASE_RELAYS, 'wss://env1.example.com', 'wss://mine.example.com', 'wss://flag.example.org']);
  assert.throws(() => resolveReleaseRelays(['ws://plain.example.com']), /bad relay/);
  assert.throws(() => resolveReleaseRelays([], 'ok.example.com,nonsense'), /bad relay/);
});

test('the publisher CLI knows --relay', () => {
  const r = spawnSync(process.execPath, [new URL('./publish-release.mjs', import.meta.url).pathname, '--relay', 'wss://x.example.com'], { encoding: 'utf8' });
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /usage: publish-release\.mjs/);
  assert.doesNotMatch(r.stderr, /unknown argument/);
});

test('the pinned release key is read from update.rs: a hex, "none" for None, null otherwise', () => {
  const hex = 'a'.repeat(64);
  assert.equal(pinnedPubkeyFrom(`pub const RELEASE_PUBKEY_HEX: Option<&str> = Some("${hex}");`), hex);
  assert.equal(pinnedPubkeyFrom('pub const RELEASE_PUBKEY_HEX: Option<&str> = None;'), 'none');
  assert.equal(pinnedPubkeyFrom('pub const RELEASE_PUBKEY_HEX: &str = "' + hex + '";'), null, 'the old Twang form is not understood');
  assert.equal(pinnedPubkeyFrom('pub const RELEASE_PUBKEY_HEX: Option<&str> = Some("short");'), null);
  assert.equal(pinnedPubkeyFrom(''), null);
  // The app's own source must stay readable by the publisher.
  const real = readFileSync(new URL('../../app/src/update.rs', import.meta.url), 'utf8');
  const got = pinnedPubkeyFrom(real);
  assert.ok(got === 'none' || /^[0-9a-f]{64}$/.test(got), `update.rs pin line not understood: ${got}`);
});

test("the channel is Stampede's and the app pins the same one", () => {
  assert.equal(RELEASE_CHANNEL, 'stampede-android-apk');
  const real = readFileSync(new URL('../../app/src/update.rs', import.meta.url), 'utf8');
  assert.match(real, /pub const RELEASE_CHANNEL: &str = "stampede-android-apk";/);
});

test('--print-pubkey prints the public half of the key file and nothing secret', async () => {
  const { mkdtempSync, writeFileSync, rmSync } = await import('node:fs');
  const { tmpdir } = await import('node:os');
  const { join } = await import('node:path');
  const { getPublicKey } = await import('nostr-tools/pure');
  const dir = mkdtempSync(join(tmpdir(), 'stampede-key-test-'));
  const secret = '11'.repeat(32); // a throwaway test key, not any real one
  const file = join(dir, 'k.hex');
  writeFileSync(file, secret + '\n');
  const r = spawnSync(process.execPath, [new URL('./publish-release.mjs', import.meta.url).pathname, '--print-pubkey'], {
    encoding: 'utf8',
    env: { ...process.env, STAMPEDE_RELEASE_KEY_FILE: file }
  });
  rmSync(dir, { recursive: true, force: true });
  assert.equal(r.status, 0, r.stderr);
  const pub = getPublicKey(Uint8Array.from(Buffer.from(secret, 'hex')));
  assert.match(r.stdout, new RegExp(`hex\\)\\s+= ${pub}`));
  assert.match(r.stdout, /npub1[0-9a-z]+/);
  assert.ok(!r.stdout.includes(secret) && !r.stderr.includes(secret), 'the secret must never be printed');
  assert.equal(r.stdout.trim().split('\n').length, 2);
});
