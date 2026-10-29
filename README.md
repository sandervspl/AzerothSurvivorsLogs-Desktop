# Azeroth Survivors Logs Desktop

Read-only distribution mirror. All development happens in the upstream monorepo.
Develop in https://github.com/sandervspl/AzerothSurvivorsLogs.
Do not submit source changes here. SOURCE_COMMIT identifies the upstream snapshot.

Download packages from this repository’s GitHub Releases. Releases are manually prepared.
Build on Windows: `cargo build --release --locked --bin azs-uploader --bin azs-update-guard`.
Default EXEs are unsigned standalone builds. The release workflow also supports configured publisher-signed installers. Authenticated catalog distribution remains separate.
