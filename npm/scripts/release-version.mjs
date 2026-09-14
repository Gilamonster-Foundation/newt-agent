import { readFileSync } from 'node:fs';

// npm normalizes away SemVer build metadata, so accepting it would allow two
// Cargo versions to publish under the same npm identity. Require an exact pin.
export function checkReleaseVersion(version) {
  const semver = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*)?$/;
  if (!semver.test(version)) {
    console.error('release-version: expected SemVer without a v prefix or +build metadata');
    process.exit(2);
  }

  const cargo = readFileSync(new URL('../../Cargo.toml', import.meta.url), 'utf8');
  const header = /^\[workspace\.package\][ \t]*(?:#.*)?\r?\n/m.exec(cargo);
  const section = header ? cargo.slice(header.index + header[0].length).split(/^\s*\[/m)[0] : '';
  const workspaceVersion = /^[ \t]*version[ \t]*=[ \t]*["']([^"']+)["'][ \t]*(?:#.*)?$/m.exec(section)?.[1];
  if (version !== workspaceVersion) {
    console.error(`release-version: requested ${version} does not match workspace version ${workspaceVersion ?? '(missing)'}`);
    process.exit(2);
  }
}
