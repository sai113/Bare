// Opens an issue when CEF publishes a stable Chromium engine newer than the one Bare uses
// (the engine it's built with, or the newest one approved in data/engines.txt).
// Run by .github/workflows/cef-watch.yml through actions/github-script.

const fs = require('fs');

const INDEX = 'https://cef-builds.spotifycdn.com/index.json';

/** "154.0.34+g14c5a08+chromium-154.0.8037.98" -> [154, 0, 34] */
function numbers(version) {
  return version.split('+')[0].split('.').map(Number);
}

function compare(a, b) {
  const [x, y] = [numbers(a), numbers(b)];
  for (let i = 0; i < Math.max(x.length, y.length); i++) {
    const d = (x[i] || 0) - (y[i] || 0);
    if (d) return d;
  }
  return 0;
}

/** The engine Bare uses now: the newer of the bundled one (Cargo.lock) and the approved one. */
function currentEngine(root) {
  const lock = fs.readFileSync(`${root}/Cargo.lock`, 'utf8');
  // (\r? so a Windows checkout with CRLF line endings works too.)
  const bundled = lock.match(/name = "cef-dll-sys"\r?\nversion = "[^+"]*\+([^"]+)"/);
  if (!bundled) throw new Error('cef-dll-sys not found in Cargo.lock');
  let current = bundled[1];
  const approvedFile = `${root}/data/engines.txt`;
  if (fs.existsSync(approvedFile)) {
    const approved = fs.readFileSync(approvedFile, 'utf8').match(/^engine=([^\r\n]+)/m);
    if (approved && compare(approved[1], current) > 0) current = approved[1];
  }
  return current;
}

/** The newest stable Windows build in CEF's index (by version, not by date). */
function newestStable(index) {
  const builds = (index.windows64 && index.windows64.versions) || [];
  return builds
    .filter((b) => b.channel === 'stable')
    .map((b) => b.cef_version)
    .sort(compare)
    .pop();
}

async function run({ github, context, core, root = '.' }) {
  const current = currentEngine(root);
  const response = await fetch(INDEX);
  if (!response.ok) throw new Error(`CEF index: HTTP ${response.status}`);
  const latest = newestStable(await response.json());
  core.info(`Bare's engine: CEF ${current}. Newest stable: CEF ${latest}.`);
  if (!latest || compare(latest, current) <= 0) return;

  const cef = latest.split('+')[0];
  const chromium = latest.split('chromium-')[1] || '?';
  const title = `New Chromium engine: CEF ${cef} (Chromium ${chromium})`;
  // One issue per version, even after it's closed.
  const { data } = await github.rest.search.issuesAndPullRequests({
    q: `repo:${context.repo.owner}/${context.repo.repo} is:issue in:title "CEF ${cef}"`,
  });
  if (data.total_count > 0) {
    core.info(`Already reported: ${title}`);
    return;
  }
  await github.rest.issues.create({
    ...context.repo,
    title,
    body: [
      `CEF has published a stable engine newer than the one Bare uses:`,
      ``,
      `- Newest: **CEF ${latest}**`,
      `- Bare uses: CEF ${current}`,
      ``,
      `New Chromium versions usually include security fixes ([Chrome release notes](https://chromereleases.googleblog.com/)), so get it to users soon:`,
      ``,
      '1. `cargo xtask engine ' + cef + '`: downloads it and builds `dist/engine-test` (Bare on that engine). Try it with **Try Bare (test profile).cmd**.',
      '2. If it works: `cargo xtask engine ' + cef + ' --approve` (signs `engines.txt`).',
      '3. Attach `dist/engines.txt` to the latest GitHub release (replacing the old one) and commit `data/engines.txt`.',
      ``,
      `Installed copies of Bare then offer it under Settings → About → Chromium engine. Close this issue once it's approved.`,
    ].join('\n'),
  });
  core.info(`Opened: ${title}`);
}

module.exports = { run, compare, currentEngine, newestStable };
