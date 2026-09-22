# Maven publication

The JDBC probe publication coordinates are
`io.github.ictechgy:schemagraph-probe:0.4.3`. The regular Maven artifact is a
thin Kotlin/JVM jar. Its POM carries Kotlin, Jackson, and bundled JDBC driver
runtime dependencies. The optional `all` classifier remains the executable
fat jar used by the fixture runner:

```text
schemagraph-probe-0.4.3.jar       thin library/application jar
schemagraph-probe-0.4.3-all.jar  executable jar with bundled drivers
schemagraph-probe-0.4.3-sources.jar
schemagraph-probe-0.4.3-javadoc.jar
```

## Install from Maven Central

Use Maven Central to resolve
[`io.github.ictechgy:schemagraph-probe:0.4.3`](https://central.sonatype.com/artifact/io.github.ictechgy/schemagraph-probe/0.4.3).
No project-specific repository URL or publishing credentials are needed:

```kotlin
repositories {
    mavenCentral()
}

dependencies {
    implementation("io.github.ictechgy:schemagraph-probe:0.4.3")
}
```

The Central and GitHub Pages releases contain identical JAR and POM bytes.
Central also carries detached PGP signatures. The v0.4.3 signing key fingerprint
is `0A98034C6F045509D1EE58EE329F94DC51434A1B`; its public key is available from
[`keyserver.ubuntu.com`](https://keyserver.ubuntu.com/pks/lookup?op=get&search=0x0A98034C6F045509D1EE58EE329F94DC51434A1B).

## Anonymous GitHub Pages Maven repository

An additional anonymous Maven repository is served from
GitHub Pages at
`https://ictechgy.github.io/schemagraph/maven/`. It is a repository for Maven
consumers and remains available alongside Maven Central.

```kotlin
repositories {
    maven { url = uri("https://ictechgy.github.io/schemagraph/maven/") }
    mavenCentral()
}

dependencies {
    implementation("io.github.ictechgy:schemagraph-probe:0.4.3")
}
```

`.github/workflows/publish-maven.yml` builds the Maven Repository Layout under
`site/maven/` and deploys it with GitHub Pages Actions after an explicit manual
version selection. It builds `probe` from the matching `vVERSION` release tag;
ordinary merges do not publish unreleased source under an existing version.
It fetches existing public Pages metadata and
artifacts before merging the current version, so previously released versions
remain available without committing binary jars to Git. It also uploads a
versioned repository zip as a workflow artifact for diagnosis.

The Pages workflow requires the repository's Pages source to use GitHub Actions
and the standard `github-pages` environment. No Central account, signing key,
or publishing token is needed for this anonymous repository.

The build defaults to version `0.4.3` and group `io.github.ictechgy`. Override
those values without editing the build:

```sh
gradle -p probe \
  -PprobeGroup=io.github.ictechgy \
  -PprobeVersion=0.4.3 \
  shadowJar
```

The normal fixture command and filename are unchanged:

```sh
gradle -p probe shadowJar
java -jar probe/build/libs/schemagraph-probe-all.jar --help
```

To create a Maven repository layout locally, point the publication at an
isolated directory:

```sh
repo_dir="$(mktemp -d)"
gradle -p probe publishMavenJavaPublicationToLocalStagingRepository \
  -PprobeRepositoryDir="$repo_dir"
```

`Scripts/verify-probe-maven.sh` performs this publication, validates the POM,
thin/all jar separation, sources and Javadoc jars, checksums, runtime
dependencies, and checks that the Javadoc jar contains generated CLI/API HTML.
It then runs the packaged CLI through an external temporary Gradle
consumer that resolves the local Maven repository. Maven CLI is used when it
is installed; Gradle is a compatible Maven repository consumer fallback.

The Pages builder validates group, artifact, and version path segments, validates
remote metadata coordinates and checksums, requires all five payloads for every
preserved version (thin, `all`, sources, Javadoc, and POM), and refuses changed
bytes when a version already exists. Re-running the same version with identical
bytes is idempotent.

## Publish to Maven Central

Maven Central is a separate release path. The existing GitHub Pages publication
remains available. You do not need a Central account, namespace,
PGP key, or token to consume or deploy the GitHub Pages repository.

Before publishing, complete these steps in the Central Portal. Never
put passwords, user tokens, or private keys in chat or in a command copied into
shell history.

### 1. Create and verify the Central account and namespace

Open [central.sonatype.com](https://central.sonatype.com/) and create an
account with an email address you can verify. The publication group is
`io.github.ictechgy`.

If the Central account is created by signing in with the GitHub user
`ictechgy`, Sonatype may automatically provision the matching personal
namespace `io.github.ictechgy`. Otherwise open the account menu, choose
**View Namespaces**, add `io.github.ictechgy`, and choose **Verify Namespace**.
For GitHub-hosted namespaces, Sonatype supplies a verification key and asks for
a temporary public repository named with that key under the owning GitHub user.
Create that repository only for verification, wait for the namespace to become
**Verified**, and remove the temporary repository afterward. Do not confirm
verification before the key is publicly visible. The exact current steps are in
[Sonatype's namespace guide](https://central.sonatype.org/register/namespace/).

### 2. Create and publish a signing key

Central requires every published payload to have a detached PGP signature. On a
trusted local machine, install GnuPG and create a primary signing key with a
passphrase. Export the public key and publish it to one of the keyservers
Central supports, such as `keys.openpgp.org` or `keyserver.ubuntu.com`; keep the
private key encrypted and backed up. Central's [PGP guide](https://central.sonatype.org/publish/requirements/gpg/)
explains key generation, primary-key selection, armored signatures, and public
key distribution.

The build accepts the armored private key only through the secret environment
variables `MAVEN_SIGNING_KEY` and optional `MAVEN_SIGNING_PASSWORD`. Inject them
from a CI secret manager or a secure, non-history interactive environment. The
repository and helper never read a local keyring or signing file.

### 3. Generate a Central user token

Visit the [Central user-token page](https://central.sonatype.com/usertoken),
choose **Generate User Token**, set a display name and expiration, and save the
credentials before closing the dialog. Sonatype does not show the token again.
Inject them only as `CENTRAL_TOKEN_USERNAME` and `CENTRAL_TOKEN_PASSWORD`; never
commit them or print them.

For the repository's manual [Central workflow](.github/workflows/publish-central.yml),
store all four values as [GitHub Actions repository secrets](https://github.com/ictechgy/schemagraph/settings/secrets/actions):

| Secret | Value |
| --- | --- |
| `CENTRAL_TOKEN_USERNAME` | Username issued with the Central user token |
| `CENTRAL_TOKEN_PASSWORD` | Password issued with the Central user token |
| `MAVEN_SIGNING_KEY` | Complete ASCII-armored private signing key |
| `MAVEN_SIGNING_PASSWORD` | Passphrase protecting that key |

These are token credentials, not your GitHub or Central login password.
The workflow requires a passphrase-protected key whose public half has already
been distributed to a supported keyserver. Secret values are injected only
into the credential check and publication steps; no private key is uploaded as
a workflow artifact.

### 4. Build and validate locally

With the signing key supplied securely, create the Central bundle. This command
does not contact Central:

```sh
Scripts/publish-probe-maven.sh --output probe/build/central-bundle-0.4.3.zip
```

The script verifies `.asc`, `.md5`, and `.sha1` sidecars for every JAR and POM,
then creates a Maven Repository Layout zip. It refuses to overwrite an
existing output. It does not contact Central unless `--upload` is provided.

### 5. Upload, validate, and publish explicitly

After checking the bundle, inject the token through the secure mechanism above
and upload it for Central validation:

```sh
Scripts/publish-probe-maven.sh --upload
```

The helper polls validation after upload and prints the final deployment JSON.
`--publish` is a separate explicit action; it publishes only after the Portal
reports `VALIDATED` and then polls/readbacks the `PUBLISHED` status:

```sh
Scripts/publish-probe-maven.sh --upload --publish \
  --poll-seconds 10 --poll-timeout 1800
```

An existing deployment can be resumed without rebuilding or rereading a
signing key:

```sh
Scripts/publish-probe-maven.sh --deployment-id <deployment-id> --publish
```

An already `PUBLISHED` deployment is treated as an idempotent readback.

The upload is user-managed and stops after validation. Use `--publish` only
after reviewing the Central validation result; the helper then waits for and
reads back `PUBLISHED`. Before publishing, verify the namespace, token
permissions, public signing key, and that `0.4.3` has not already been released.
Central releases are immutable.

Alternatively, run the manual workflow from `main` after the four secrets are
configured:

```sh
gh workflow run publish-central.yml --ref main -f version=0.4.3 -f publish=true
```

It builds the probe from the existing `v0.4.3` tag, compares all five unsigned
payloads byte-for-byte with the public Pages repository, then uploads the signed
bundle. Publication proceeds only after Central reports `VALIDATED`, and the
workflow waits for `PUBLISHED`. With `publish=false` (the default), it stops
after validation. The workflow keeps the signed public bundle for 30 days.
If polling times out after upload, use the deployment ID in the log to inspect
or publish that deployment in the Portal rather than blindly uploading again.

The same byte comparison is available in the local helper with
`--compare-repository https://ictechgy.github.io/schemagraph/maven/`.
`PROBE_SOURCE_DIR` can select the `probe` directory in a separate release
checkout while using the current publication helper.

The publication metadata includes the MIT and Apache-2.0 licenses, project
description and URL, developer information, and Git SCM coordinates. Central
also requires sources, Javadoc, checksums, signatures, and a non-SNAPSHOT
version; the scripts check these requirements before any optional upload.

The relevant official references are [Gradle Maven Publish](https://docs.gradle.org/current/userguide/publishing_maven.html),
[Gradle Signing](https://docs.gradle.org/current/userguide/publishing_signing.html),
[Central account registration](https://central.sonatype.org/register/central-portal/),
[namespace verification](https://central.sonatype.org/register/namespace/),
[user tokens](https://central.sonatype.org/publish/generate-portal-token/),
[PGP requirements](https://central.sonatype.org/publish/requirements/gpg/), and
[Publisher API](https://central.sonatype.org/publish/publish-portal-api/).
