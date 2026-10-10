"""Pinned digests for a managed Ollama runtime archive, computed from the archive.

usage: python scripts/runtime-tree-digest.py ARCHIVE EXECUTABLE

Prints the archive SHA-256, the executable's SHA-256 and the tree digest that
phonton-local's `tree_sha256` computes after extraction: every regular file as
`path NUL sha256 LF` and, on Unix, every symlink as `path NUL link:target LF`,
sorted by path. Needs `pip install zstandard` for .tar.zst archives. Tarballs
are streamed, so multi-gigabyte archives need little memory.
"""

import hashlib
import sys
import tarfile
import zipfile


def sha256_stream(stream):
    digest = hashlib.sha256()
    for chunk in iter(lambda: stream.read(1 << 20), b""):
        digest.update(chunk)
    return digest.hexdigest()


def tar_entries(path):
    raw = open(path, "rb")
    if path.endswith(".tar.zst"):
        import zstandard

        stream = zstandard.ZstdDecompressor().stream_reader(raw, read_across_frames=True)
        archive = tarfile.open(fileobj=stream, mode="r|")
    else:
        archive = tarfile.open(fileobj=raw, mode="r|*")
    hashes = {}
    with archive:
        for member in archive:
            name = member.name.removeprefix("./")
            if member.isdir() or not name:
                continue
            if member.issym():
                hashes[name] = "link:" + member.linkname
            elif member.islnk():
                hashes[name] = hashes[member.linkname.removeprefix("./")]
            elif member.isfile():
                hashes[name] = sha256_stream(archive.extractfile(member))
            else:
                raise SystemExit(f"unexpected entry type: {member.name}")
    raw.close()
    return hashes


def zip_entries(path):
    with zipfile.ZipFile(path) as archive:
        return {
            info.filename: sha256_stream(archive.open(info))
            for info in archive.infolist()
            if not info.is_dir()
        }


def main():
    path, executable = sys.argv[1], sys.argv[2]
    with open(path, "rb") as f:
        archive_hash = sha256_stream(f)
    entries = zip_entries(path) if path.endswith(".zip") else tar_entries(path)
    tree = hashlib.sha256()
    for name in sorted(entries):
        tree.update(name.encode() + b"\0" + entries[name].encode() + b"\n")
    print(f"archive    {archive_hash}")
    print(f"executable {entries[executable]}")
    print(f"tree       {tree.hexdigest()}  ({len(entries)} entries)")


if __name__ == "__main__":
    main()
