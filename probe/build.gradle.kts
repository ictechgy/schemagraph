plugins {
    kotlin("jvm") version "2.3.21"
    id("com.gradleup.shadow") version "9.2.2"
    application
}

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
}

kotlin {
    jvmToolchain(17)
}

application {
    mainClass.set("schemagraph.probe.MainKt")
}

tasks.shadowJar {
    archiveBaseName.set("schemagraph-probe")
    mergeServiceFiles()
}
