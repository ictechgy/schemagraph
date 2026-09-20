# Maven publication

The JDBC probe publication coordinates are
`io.github.ictechgy:schemagraph-probe:0.3.0`. The regular Maven artifact is a
thin Kotlin/JVM jar. Its POM carries Kotlin, Jackson, and bundled JDBC driver
runtime dependencies. The optional `all` classifier remains the executable
fat jar used by the fixture runner:

```text
schemagraph-probe-0.3.0.jar       thin library/application jar
schemagraph-probe-0.3.0-all.jar  executable jar with bundled drivers
schemagraph-probe-0.3.0-sources.jar
schemagraph-probe-0.3.0-javadoc.jar
```

The build defaults to version `0.3.0` and group `io.github.ictechgy`. Override
those values without editing the build:

```sh
gradle -p probe \
  -PprobeGroup=io.github.ictechgy \
  -PprobeVersion=0.3.0 \
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

Central validation requires a signed release bundle. The signing plugin accepts
an ASCII-armored private key in `MAVEN_SIGNING_KEY` and an optional
`MAVEN_SIGNING_PASSWORD`; no key files or credential stores are read. Inject
these values from a CI secret manager or an interactive environment that does
not record shell history. Build and validate a Central bundle with:

```sh
Scripts/publish-probe-maven.sh --output probe/build/central-bundle-0.3.0.zip
```

The script verifies `.asc`, `.md5`, and `.sha1` sidecars for every JAR and POM,
then creates a Maven Repository Layout zip. It refuses to overwrite an
existing output. It does not contact Central unless `--upload` is provided.
For an explicitly authorized upload, inject a Central user token through
`CENTRAL_TOKEN_USERNAME` and `CENTRAL_TOKEN_PASSWORD` from the same secure
secret mechanism:

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

The upload only submits the bundle for Central validation. The deployment still
needs the Portal's publish action unless an account owner chooses an automatic
publishing workflow. Before publishing, the account owner must verify the
`io.github.ictechgy` namespace, token permissions, a public signing key on a
keyserver, and that `0.3.0` has not already been released. Central releases are
immutable.

The publication metadata includes the MIT and Apache-2.0 licenses, project
description and URL, developer information, and Git SCM coordinates. Central
also requires sources, Javadoc, checksums, signatures, and a non-SNAPSHOT
version; the scripts check these requirements before any optional upload.

The relevant official references are [Gradle Maven Publish](https://docs.gradle.org/current/userguide/publishing_maven.html),
[Gradle Signing](https://docs.gradle.org/current/userguide/publishing_signing.html),
[Central requirements](https://central.sonatype.org/publish/requirements/), and
[Central bundle upload](https://central.sonatype.org/publish/publish-portal-upload/).
