# Azeroth Survivors Logs Desktop

Read-only distribution mirror. All development happens in the upstream monorepo.
Develop in https://github.com/sandervspl/AzerothSurvivorsLogs.
Do not submit source changes here. SOURCE_COMMIT identifies the upstream snapshot.

Download packages from this repository’s GitHub Releases. Releases are manually prepared.
Build on Windows: `cargo build --release --locked --bin azs-uploader --bin azs-update-guard`.
The release workflow defaults to publisher-signed managed installers; unsigned standalone EXEs require an explicit opt-out.
The signed update catalog lives on this mirror's separate updates branch. Publishing a versioned release does not promote that catalog.
