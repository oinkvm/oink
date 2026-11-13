// Android Gradle dependencies (licenses/android-runtime.json, written by `:app:oinkLicenseInventory`)
// → license components. Pure: the generator passes the reviewed policy (licenses/maven-licenses.json),
// the allowed SPDX list (deny.toml) and a file reader for the reviewed full texts.
import { createHash } from 'node:crypto';
import { selectLicense } from './license-policy.mjs';

const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const googleMaven = /^(androidx|com\.android|com\.google\.android)\./;

export function mavenComponents(gradle, policy, allowed, readFile) {
  if (gradle.schemaVersion !== 1 || gradle.configuration !== 'releaseRuntimeClasspath') throw Error('Unexpected Android runtime inventory');
  for (const [sha, text] of Object.entries(gradle.texts)) if (hash(Buffer.from(text, 'utf8')) !== sha) throw Error(`Android legal text hash changed: ${sha}`);
  return gradle.components.map(m => {
    const id = `maven:${m.group}:${m.name}@${m.version}`;
    const review = policy.reviews.find(r => r.group === m.group && r.name === m.name && r.version === m.version);
    let licenseExpression;
    if (review) {
      if (JSON.stringify(review.pomLicenses) !== JSON.stringify(m.pomLicenses)) throw Error(`Maven license review no longer matches the POM: ${id}`);
      licenseExpression = review.licenseExpression;
    } else {
      if (m.pomLicenses.length !== 1) throw Error(`POM declares ${m.pomLicenses.length} licenses; review the expression in licenses/maven-licenses.json: ${id}`);
      licenseExpression = policy.names[m.pomLicenses[0].name];
      if (!licenseExpression) throw Error(`Unknown Maven POM license "${m.pomLicenses[0].name}" (${m.pomLicenses[0].url}): ${id}; map it in licenses/maven-licenses.json`);
    }
    const selectedLicense = selectLicense(licenseExpression, allowed);
    const shipped = m.legalFiles.map(f => {
      const text = gradle.texts[f.sha256];
      if (text === undefined) throw Error(`Missing Android legal text: ${id} ${f.path}`);
      return {...f, text};
    });
    // Shipped LICENSE/NOTICE files are kept verbatim. POM-only licenses (most AndroidX and Kotlin
    // artifacts ship no text) get the reviewed full text; other licenses need the artifact's own.
    const shipsLicense = shipped.some(f => /(licen[cs]e|copying)/i.test(f.path.split('/').pop()));
    const reviewed = selectedLicense.replace(/[()]/g, '').split(' AND ').flatMap(spdx => {
      const file = policy.texts[spdx];
      if (!file) {
        if (!shipsLicense) throw Error(`No license text for ${spdx}: ${id}; the artifact ships none and licenses/maven-licenses.json has no reviewed text`);
        return [];
      }
      const bytes = readFile(file.path);
      if (hash(bytes) !== file.sha256) throw Error(`Reviewed Maven license text changed: ${file.path}`);
      return [bytes.toString('utf8')];
    });
    const artifact = m.artifacts[0];
    const repository = googleMaven.test(`${m.group}.`) ? 'https://dl.google.com/dl/android/maven2' : 'https://repo1.maven.org/maven2';
    return {id, name: `${m.group}:${m.name}`, version: m.version, repositoryUrls: [m.scm, m.url],
      source: {url: `${repository}/${m.group.replaceAll('.', '/')}/${m.name}/${m.version}/${artifact.file}`, integrity: `sha256:${artifact.sha256}`},
      licenseExpression, selectedLicense, texts: [...shipped.map(f => f.text), ...reviewed]};
  });
}
