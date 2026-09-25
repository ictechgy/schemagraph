import org.gradle.jvm.tasks.Jar
import org.gradle.api.tasks.bundling.AbstractArchiveTask

plugins {
    kotlin("jvm") version "2.3.21"
    id("com.gradleup.shadow") version "9.2.2"
    application
    `maven-publish`
    signing
}

group = providers.gradleProperty("probeGroup").orElse("io.github.ictechgy").get()
version = providers.gradleProperty("probeVersion").orElse("0.6.0").get()

repositories {
    mavenCentral()
}

dependencies {
    implementation("com.fasterxml.jackson.module:jackson-module-kotlin:2.20.1")

    // 번들 드라이버는 허용 라이선스만 — MySQL(GPL)·Oracle(OTN) 등은
    // --driver로 사용자가 jar을 넘긴다. pgjdbc=BSD, H2=EPL/MPL,
    // sqlite=Apache, mssql-jdbc=MIT.
    runtimeOnly("org.postgresql:postgresql:42.7.8")
    runtimeOnly("com.h2database:h2:2.4.240")
    runtimeOnly("org.xerial:sqlite-jdbc:3.51.1.0")
    runtimeOnly("com.microsoft.sqlserver:mssql-jdbc:13.4.0.jre11")

    testImplementation(kotlin("test"))
}

kotlin {
    jvmToolchain(17)
}

application {
    mainClass.set("schemagraph.probe.MainKt")
}

java {
    withSourcesJar()
    withJavadocJar()
}

val probeJavadocDir = layout.buildDirectory.dir("generated/probe-javadoc")
val generateProbeJavadoc = tasks.register("generateProbeJavadoc") {
    outputs.dir(probeJavadocDir)
    doLast {
        val directory = probeJavadocDir.get().asFile
        directory.mkdirs()
        directory.resolve("index.html").writeText(
            """
            <!doctype html>
            <html lang="en">
            <head><meta charset="UTF-8"><title>schemagraph-probe</title></head>
            <body>
            <h1>schemagraph-probe</h1>
            <p>JDBC catalog probe used by schemagraph.</p>
            <h2>CLI entry point</h2>
            <p><code>schemagraph.probe.MainKt</code> accepts a JDBC URL and emits
            a JSON or NDJSON catalog document.</p>
            <pre>java -jar schemagraph-probe-all.jar --url jdbc:sqlite:/path/db -o catalog.json</pre>
            <h2>Library entry points</h2>
            <ul>
              <li><code>schemagraph.probe.Extractor</code> collects a catalog from a JDBC connection.</li>
              <li><code>schemagraph.probe.CatalogDocument</code> is the wire model emitted by the CLI.</li>
            </ul>
            <p>See the project documentation at
            <a href="https://github.com/ictechgy/schemagraph">github.com/ictechgy/schemagraph</a>.</p>
            </body>
            </html>
            """.trimIndent()
        )
    }
}

tasks.named<Jar>("javadocJar") {
    dependsOn(generateProbeJavadoc)
    from(probeJavadocDir)
}

tasks.test {
    useJUnitPlatform()
}

// 카탈로그 조회문은 Rust source의 정본을 패키징해 JDBC와 의미가 갈라지지 않게 한다.
tasks.processResources {
    from("../engine/source/src/sql") { into("catalog") }
}

tasks.withType<AbstractArchiveTask>().configureEach {
    isPreserveFileTimestamps = false
    isReproducibleFileOrder = true
}

tasks.shadowJar {
    archiveBaseName.set("schemagraph-probe")
    archiveClassifier.set("all")
    mergeServiceFiles()
    manifest {
        attributes["Main-Class"] = application.mainClass.get()
    }
    // 기존 fixture runner는 versionless 이름을 사용하므로, Maven용 버전 달린
    // artifact와 함께 호환용 실행 파일 이름도 shadow 작업에서 갱신한다.
    val legacyJar = layout.buildDirectory.file("libs/schemagraph-probe-all.jar")
    outputs.file(legacyJar)
    doLast {
        archiveFile.get().asFile.copyTo(legacyJar.get().asFile, overwrite = true)
    }
}

val probeRepositoryDir = providers.gradleProperty("probeRepositoryDir")
    .map { file(it) }
    .orElse(layout.buildDirectory.dir("maven-repository").map { it.asFile })

publishing {
    publications {
        create<MavenPublication>("mavenJava") {
            artifactId = "schemagraph-probe"
            from(components["java"])
            pom {
                name = "schemagraph-probe"
                description = "JDBC catalog probe for schemagraph"
                url = "https://github.com/ictechgy/schemagraph"
                licenses {
                    license {
                        name = "MIT License"
                        url = "https://opensource.org/licenses/MIT"
                        distribution = "repo"
                    }
                    license {
                        name = "Apache License, Version 2.0"
                        url = "https://www.apache.org/licenses/LICENSE-2.0.txt"
                        distribution = "repo"
                    }
                }
                developers {
                    developer {
                        id = "ictechgy"
                        name = "schemagraph authors"
                        organization = "ictechgy"
                        organizationUrl = "https://github.com/ictechgy"
                    }
                }
                scm {
                    connection = "scm:git:https://github.com/ictechgy/schemagraph.git"
                    developerConnection = "scm:git:ssh://git@github.com/ictechgy/schemagraph.git"
                    url = "https://github.com/ictechgy/schemagraph/tree/main"
                    tag = "v${project.version}"
                }
            }
        }
    }
    repositories {
        maven {
            name = "localStaging"
            url = uri(probeRepositoryDir.get())
        }
    }
}

val signingKey = providers.environmentVariable("MAVEN_SIGNING_KEY")
    .orElse(providers.gradleProperty("signingKey"))
val signingPassword = providers.environmentVariable("MAVEN_SIGNING_PASSWORD")
    .orElse(providers.gradleProperty("signingPassword"))
val signingRequired = providers.gradleProperty("signingRequired")
    .map(String::toBoolean)
    .orElse(false)

signing {
    isRequired = signingRequired.get()
    if (signingKey.isPresent) {
        useInMemoryPgpKeys(signingKey.get(), signingPassword.orNull)
    }
    sign(publishing.publications["mavenJava"])
}
