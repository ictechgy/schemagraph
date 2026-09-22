#!/usr/bin/env python3
"""Build an anonymous Maven repository tree for GitHub Pages.

The repository is generated in CI and is never committed to Git. Existing
released versions are fetched from the public Pages URL before the current
publication is merged, so a later workflow run does not remove old binaries.
"""

from __future__ import annotations

import argparse
import contextlib
import datetime as dt
import hashlib
import html
import http.server
import os
import pathlib
import re
import shutil
import sys
import tempfile
import threading
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
import zipfile


PAYLOAD_SUFFIXES = (".pom", ".jar")
REQUIRED_CLASSIFIERS = ("", "-all", "-sources", "-javadoc")
SIDECAR_SUFFIXES = (".asc", ".md5", ".sha1", ".sha256", ".sha512")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--publication-root", type=pathlib.Path)
    parser.add_argument("--site-dir", type=pathlib.Path)
    parser.add_argument("--group", default="io.github.ictechgy")
    parser.add_argument("--artifact", default="schemagraph-probe")
    parser.add_argument("--version", default="0.5.0")
    parser.add_argument(
        "--existing-base-url",
        default="https://ictechgy.github.io/schemagraph/maven/",
        help="anonymous Maven Pages root used to preserve previous versions",
    )
    parser.add_argument(
        "--no-existing",
        action="store_true",
        help="do not fetch previous Pages versions",
    )
    parser.add_argument("--archive-output", type=pathlib.Path)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument(
        "--source-date-epoch",
        type=int,
        default=int(os.environ.get("SOURCE_DATE_EPOCH", "0")),
    )
    return parser.parse_args()


def group_path(group: str) -> str:
    return group.replace(".", "/")


def validate_segment(value: str, label: str) -> None:
    if (
        not value
        or value in {".", ".."}
        or "/" in value
        or "\\" in value
        or any(ord(character) < 32 for character in value)
    ):
        raise RuntimeError(f"invalid {label} path segment: {value!r}")


def validate_coordinates(group: str, artifact: str, version: str) -> None:
    if not group or any(not part for part in group.split(".")):
        raise RuntimeError(f"invalid group path: {group!r}")
    for part in group.split("."):
        validate_segment(part, "group")
    validate_segment(artifact, "artifact")
    validate_segment(version, "version")


def payload_names(artifact: str, version: str) -> list[str]:
    return [f"{artifact}-{version}.pom"] + [
        f"{artifact}-{version}{classifier}.jar" for classifier in REQUIRED_CLASSIFIERS
    ]


def version_key(version: str) -> tuple[tuple[int, object], ...]:
    return tuple(
        (0, int(part)) if part.isdigit() else (1, part)
        for part in re.split(r"[.-]", version)
    )


def artifact_root(site_dir: pathlib.Path, group: str, artifact: str) -> pathlib.Path:
    return site_dir / "maven" / group_path(group) / artifact


def download(url: str) -> bytes | None:
    request = urllib.request.Request(url, headers={"User-Agent": "schemagraph-maven-site/1"})
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise RuntimeError(f"HTTP {error.code} while reading {url}") from error
    except urllib.error.URLError as error:
        raise RuntimeError(f"cannot read {url}: {error.reason}") from error


def existing_versions(base_url: str, group: str, artifact: str) -> list[str]:
    validate_coordinates(group, artifact, "0.0.0")
    metadata_url = f"{base_url.rstrip('/')}/{group_path(group)}/{artifact}/maven-metadata.xml"
    raw = download(metadata_url)
    if raw is None:
        return []
    try:
        root = ET.fromstring(raw)
    except ET.ParseError as error:
        raise RuntimeError(f"invalid existing Maven metadata {metadata_url}: {error}") from error
    existing_group = root.findtext("./groupId")
    existing_artifact = root.findtext("./artifactId")
    if existing_group != group or existing_artifact != artifact:
        raise RuntimeError(
            f"existing Maven metadata coordinates are {existing_group!r}:{existing_artifact!r}, "
            f"expected {group}:{artifact}"
        )
    versions = {
        value.text for value in root.findall("./versioning/versions/version") if value.text
    }
    for version in versions:
        validate_segment(version, "existing version")
    return sorted(
        versions,
        key=version_key,
    )


def copy_existing_versions(
    site_root: pathlib.Path,
    base_url: str,
    group: str,
    artifact: str,
) -> list[str]:
    versions = existing_versions(base_url, group, artifact)
    for version in versions:
        validate_coordinates(group, artifact, version)
        destination = site_root / version
        destination.mkdir(parents=True, exist_ok=True)
        for name in payload_names(artifact, version):
            url = f"{base_url.rstrip('/')}/{group_path(group)}/{artifact}/{version}/{name}"
            raw = download(url)
            if raw is None:
                raise RuntimeError(f"existing Pages version {version} is missing {name}")
            (destination / name).write_bytes(raw)
            for suffix in (".md5", ".sha1"):
                checksum_url = url + suffix
                checksum = download(checksum_url)
                if checksum is None:
                    raise RuntimeError(
                        f"existing Pages version {version} is missing {name}{suffix}"
                    )
                verify_checksum(raw, checksum, suffix, f"{checksum_url}")
                (destination / f"{name}{suffix}").write_bytes(checksum)
            for suffix in (".asc", ".sha256", ".sha512"):
                optional = download(url + suffix)
                if optional is not None:
                    if suffix in {".sha256", ".sha512"}:
                        verify_checksum(raw, optional, suffix, f"{url}{suffix}")
                    (destination / f"{name}{suffix}").write_bytes(optional)
    return versions


def verify_checksum(payload: bytes, checksum: bytes, suffix: str, source: str) -> None:
    try:
        expected = checksum.decode("ascii").strip().split()[0].lower()
    except (UnicodeDecodeError, IndexError) as error:
        raise RuntimeError(f"invalid checksum file {source}") from error
    algorithm = suffix.removeprefix(".")
    actual = hashlib.new(algorithm, payload).hexdigest()
    if actual != expected:
        raise RuntimeError(f"checksum mismatch for preserved file {source}")


def copy_current_publication(
    publication_root: pathlib.Path,
    destination: pathlib.Path,
    group: str,
    artifact: str,
    version: str,
) -> None:
    validate_coordinates(group, artifact, version)
    source = publication_root / group_path(group) / artifact / version
    if not source.is_dir():
        raise RuntimeError(f"publication directory is missing: {source}")
    required = {
        *payload_names(artifact, version),
    }
    present = {path.name for path in source.iterdir() if path.is_file()}
    if not required.issubset(present):
        raise RuntimeError(f"current publication is missing required files: {sorted(required - present)}")
    destination.mkdir(parents=True, exist_ok=True)
    existing_payloads = {path.name for path in destination.iterdir() if path.is_file()}
    if existing_payloads:
        for name in required:
            current = source / name
            existing = destination / name
            if not existing.is_file():
                raise RuntimeError(f"existing version is missing immutable payload {name}")
            if current.read_bytes() != existing.read_bytes():
                raise RuntimeError(
                    f"immutable Maven version {version} changed bytes for {name}"
                )
        # 같은 바이트의 재실행은 기존 서명과 체크섬도 그대로 보존한다.
        return
    allowed = {name + suffix for name in required for suffix in ("",) + SIDECAR_SUFFIXES}
    for path in source.iterdir():
        if path.is_file() and path.name in allowed:
            shutil.copy2(path, destination / path.name)


def metadata_timestamp(epoch: int) -> str:
    if epoch <= 0:
        return "19700101000000"
    return dt.datetime.fromtimestamp(epoch, dt.timezone.utc).strftime("%Y%m%d%H%M%S")


def write_metadata(root: pathlib.Path, group: str, artifact: str, versions: list[str], epoch: int) -> None:
    metadata = ET.Element("metadata")
    ET.SubElement(metadata, "groupId").text = group
    ET.SubElement(metadata, "artifactId").text = artifact
    versioning = ET.SubElement(metadata, "versioning")
    if versions:
        ET.SubElement(versioning, "latest").text = versions[-1]
        ET.SubElement(versioning, "release").text = versions[-1]
    ET.SubElement(versioning, "lastUpdated").text = metadata_timestamp(epoch)
    values = ET.SubElement(versioning, "versions")
    for version in versions:
        ET.SubElement(values, "version").text = version
    tree = ET.ElementTree(metadata)
    ET.indent(tree, space="  ")
    path = root / "maven-metadata.xml"
    tree.write(path, encoding="utf-8", xml_declaration=True)
    write_sidecars(path)


def write_sidecars(path: pathlib.Path) -> None:
    data = path.read_bytes()
    for algorithm, suffix in (("md5", ".md5"), ("sha1", ".sha1"), ("sha256", ".sha256"), ("sha512", ".sha512")):
        path.with_name(path.name + suffix).write_text(
            hashlib.new(algorithm, data).hexdigest() + "\n", encoding="ascii"
        )


def write_indexes(site_dir: pathlib.Path, group: str, artifact: str, versions: list[str]) -> None:
    root = site_dir / "maven"
    root.mkdir(parents=True, exist_ok=True)
    root_index = root / "index.html"
    artifact_url = f"{group_path(group)}/{artifact}/"
    root_index.write_text(
        "<!doctype html><meta charset='utf-8'><title>schemagraph Maven repository</title>"
        f"<p>Anonymous Maven repository for <a href='{html.escape(artifact_url)}'>"
        f"{html.escape(group)}:{html.escape(artifact)}</a>.</p>\n",
        encoding="utf-8",
    )
    artifact_root_path = root / group_path(group) / artifact
    links = "\n".join(
        f"<li><a href='{html.escape(version)}/'>{html.escape(version)}</a></li>"
        for version in versions
    )
    (artifact_root_path / "index.html").write_text(
        "<!doctype html><meta charset='utf-8'><title>schemagraph-probe Maven versions</title>"
        f"<h1>{html.escape(group)}:{html.escape(artifact)}</h1><ul>{links}</ul>\n",
        encoding="utf-8",
    )
    for version in versions:
        directory = artifact_root_path / version
        files = "\n".join(
            f"<li><a href='{html.escape(name)}'>{html.escape(name)}</a></li>"
            for name in payload_names(artifact, version)
        )
        (directory / "index.html").write_text(
            "<!doctype html><meta charset='utf-8'><title>schemagraph Maven downloads</title>"
            f"<h1>{html.escape(artifact)} {html.escape(version)}</h1><ul>{files}</ul>\n",
            encoding="utf-8",
        )


def write_site_index(site_dir: pathlib.Path) -> None:
    site_dir.mkdir(parents=True, exist_ok=True)
    (site_dir / "index.html").write_text(
        "<!doctype html><meta charset='utf-8'><title>schemagraph</title>"
        "<p>Public Maven repository: <a href='maven/'>maven/</a>.</p>\n",
        encoding="utf-8",
    )


def write_archive(site_dir: pathlib.Path, output: pathlib.Path, epoch: int) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    timestamp = dt.datetime.fromtimestamp(max(epoch, 315532800), dt.timezone.utc)
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for path in sorted((site_dir / "maven").rglob("*")):
            if path.is_file():
                info = zipfile.ZipInfo(path.relative_to(site_dir).as_posix())
                info.date_time = timestamp.timetuple()[:6]
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = 0o644 << 16
                archive.writestr(info, path.read_bytes())


def build_site(args: argparse.Namespace) -> list[str]:
    validate_coordinates(args.group, args.artifact, args.version)
    if args.version.endswith("-SNAPSHOT"):
        raise SystemExit("error: GitHub Pages Maven versions must not end in -SNAPSHOT")
    site_dir = args.site_dir
    site_dir.mkdir(parents=True, exist_ok=True)
    root = artifact_root(site_dir, args.group, args.artifact)
    previous = []
    if not args.no_existing:
        previous = copy_existing_versions(
            root,
            args.existing_base_url,
            args.group,
            args.artifact,
        )
    publication_versions = set(previous)
    copy_current_publication(
        args.publication_root,
        root / args.version,
        args.group,
        args.artifact,
        args.version,
    )
    publication_versions.add(args.version)
    versions = sorted(publication_versions, key=version_key)
    write_metadata(root, args.group, args.artifact, versions, args.source_date_epoch)
    write_indexes(site_dir, args.group, args.artifact, versions)
    write_site_index(site_dir)
    if args.archive_output:
        write_archive(site_dir, args.archive_output, args.source_date_epoch)
    print(f"built anonymous Maven site with versions: {', '.join(versions)}")
    return versions


@contextlib.contextmanager
def local_http_site(root: pathlib.Path):
    class QuietHandler(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *_args):
            pass

    handler = lambda *args, **kwargs: QuietHandler(*args, directory=str(root), **kwargs)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}/maven/"
    finally:
        server.shutdown()
        thread.join(timeout=5)


def write_test_publication(root: pathlib.Path, group: str, artifact: str, version: str, marker: bytes) -> None:
    destination = root / group_path(group) / artifact / version
    destination.mkdir(parents=True, exist_ok=True)
    for name in payload_names(artifact, version):
        payload = marker + b":" + name.encode("ascii")
        path = destination / name
        path.write_bytes(payload)
        for algorithm, suffix in (("md5", ".md5"), ("sha1", ".sha1")):
            path.with_name(path.name + suffix).write_text(
                hashlib.new(algorithm, payload).hexdigest() + "\n", encoding="ascii"
            )


def write_test_metadata(root: pathlib.Path, group: str, artifact: str, versions: list[str]) -> None:
    write_metadata(root / group_path(group) / artifact, group, artifact, versions, 1700000000)


def self_test() -> None:
    group = "io.github.ictechgy"
    artifact = "schemagraph-probe"
    with tempfile.TemporaryDirectory(prefix="maven-site-self-test-") as temporary:
        root = pathlib.Path(temporary)
        previous = root / "previous"
        publication = root / "publication"
        write_test_publication(previous / "maven", group, artifact, "0.2.0", b"old")
        write_test_metadata(previous / "maven", group, artifact, ["0.2.0"])
        write_test_publication(publication, group, artifact, "0.3.0", b"current")
        (publication / group_path(group) / artifact / "0.3.0" / "unrelated.txt").write_text("fixture")

        with local_http_site(previous) as base_url:
            merged = root / "merged"
            build_site(argparse.Namespace(
                publication_root=publication,
                site_dir=merged,
                group=group,
                artifact=artifact,
                version="0.3.0",
                existing_base_url=base_url,
                no_existing=False,
                archive_output=None,
                source_date_epoch=1700000000,
                self_test=False,
            ))
            assert (merged / "maven" / group_path(group) / artifact / "0.2.0" / payload_names(artifact, "0.2.0")[1]).is_file()
            version_root = merged / "maven" / group_path(group) / artifact / "0.3.0"
            assert (version_root / "index.html").is_file()
            assert not (version_root / "unrelated.txt").exists()

            bad_checksum = previous / "maven" / group_path(group) / artifact / "0.2.0" / f"{artifact}-0.2.0.jar.md5"
            bad_checksum.write_text("0" * 32 + "\n", encoding="ascii")
            try:
                build_site(argparse.Namespace(
                    publication_root=publication, site_dir=root / "bad-checksum", group=group,
                    artifact=artifact, version="0.3.0", existing_base_url=base_url,
                    no_existing=False, archive_output=None, source_date_epoch=1700000000,
                    self_test=False,
                ))
            except RuntimeError as error:
                assert "checksum" in str(error)
            else:
                raise AssertionError("bad checksum was accepted")

        missing = root / "missing"
        write_test_publication(missing / "maven", group, artifact, "0.2.0", b"old")
        (missing / "maven" / group_path(group) / artifact / "0.2.0" / f"{artifact}-0.2.0-all.jar").unlink()
        write_test_metadata(missing / "maven", group, artifact, ["0.2.0"])
        with local_http_site(missing) as base_url:
            try:
                build_site(argparse.Namespace(
                    publication_root=publication, site_dir=root / "missing-output", group=group,
                    artifact=artifact, version="0.3.0", existing_base_url=base_url,
                    no_existing=False, archive_output=None, source_date_epoch=1700000000,
                    self_test=False,
                ))
            except RuntimeError as error:
                assert "all.jar" in str(error)
            else:
                raise AssertionError("missing previous payload was accepted")

        immutable = root / "immutable"
        build_site(argparse.Namespace(
            publication_root=publication, site_dir=immutable, group=group,
            artifact=artifact, version="0.3.0", existing_base_url="", no_existing=True,
            archive_output=None, source_date_epoch=1700000000, self_test=False,
        ))
        build_site(argparse.Namespace(
            publication_root=publication, site_dir=immutable, group=group,
            artifact=artifact, version="0.3.0", existing_base_url="", no_existing=True,
            archive_output=None, source_date_epoch=1700000000, self_test=False,
        ))
        changed_publication = root / "changed"
        write_test_publication(changed_publication, group, artifact, "0.3.0", b"changed")
        try:
            build_site(argparse.Namespace(
                publication_root=changed_publication, site_dir=immutable, group=group,
                artifact=artifact, version="0.3.0", existing_base_url="", no_existing=True,
                archive_output=None, source_date_epoch=1700000000, self_test=False,
            ))
        except RuntimeError as error:
            assert "immutable Maven version" in str(error)
        else:
            raise AssertionError("changed immutable bytes were accepted")

        traversal = root / "traversal"
        traversal.mkdir(parents=True)
        (traversal / "maven").mkdir()
        metadata = traversal / "maven" / "maven-metadata.xml"
        # The artifact URL is intentionally valid; the metadata version is not.
        artifact_metadata = traversal / "maven" / group_path(group) / artifact
        artifact_metadata.mkdir(parents=True)
        (artifact_metadata / "maven-metadata.xml").write_text(
            "<metadata><groupId>io.github.ictechgy</groupId><artifactId>schemagraph-probe</artifactId>"
            "<versioning><versions><version>../escape</version></versions></versioning></metadata>",
            encoding="utf-8",
        )
        with local_http_site(traversal) as base_url:
            try:
                build_site(argparse.Namespace(
                    publication_root=publication, site_dir=root / "traversal-output", group=group,
                    artifact=artifact, version="0.3.0", existing_base_url=base_url,
                    no_existing=False, archive_output=None, source_date_epoch=1700000000,
                    self_test=False,
                ))
            except RuntimeError as error:
                assert "existing version" in str(error) or "path segment" in str(error)
            else:
                raise AssertionError("traversal metadata was accepted")
    print("Maven site self-tests passed: traversal, checksum, missing payload, immutable bytes, idempotent merge")


def main() -> int:
    args = parse_args()
    if args.self_test:
        self_test()
        return 0
    if args.publication_root is None or args.site_dir is None:
        raise SystemExit("error: --publication-root and --site-dir are required")
    build_site(args)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RuntimeError as error:
        raise SystemExit(f"error: {error}") from error
