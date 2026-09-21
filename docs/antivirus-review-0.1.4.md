# Auricle 0.1.4 antivirus review

Investigation date: 2026-09-15. This is an evidence record, not a safety certification.
The published release binaries were not rebuilt, modified, signed, or replaced.

## Status checked 2026-09-21

- Microsoft has determined the installer is **Not malware**, with analyst comments
  explicitly confirming removal of the detection. SecureAge confirmed successful
  processing of the false-positive report by email on September 16; details below.
- Before requesting today's reanalysis, VirusTotal displayed **1/60** from September 18:
  Microsoft **Undetected**, SecureAge **Malicious**, and Skyhigh (SWG) a scan **failure**.
  A failed engine is not a clean verdict. The completed September 21 reanalysis now
  shows **1/70**: Microsoft and SecureAge **Undetected**, with only Skyhigh (SWG)
  reporting `BehavesLike.Win32.ObfuscatedPoly.vc`. This confirms SecureAge's update
  has propagated to this scan, not an overall safety certification.
- GitHub release 0.1.4, SourceForge's default 0.1.4 download, the custom Scoop bucket,
  and GitHub Pages remain live. GitHub/Scoop asset hashes are unchanged.
- WinGet PR 434000 remains open, with no update since September 13. Installation
  validation is neutral and requires manual review.
- Chocolatey's reviewer accepted the icon URL and checksum and requested copyright
  metadata. The copyright-only 0.1.4 replacement was built and verified, preserving
  every other metadata field and both scripts. After refreshing the saved API key,
  resubmission succeeded on September 21 and the downloadable nupkg was verified.
  The reviewer note is saved; automated checks and moderator review are pending.
  No installer rebuild or version bump was made.

## Scan results from 2026-09-15

- Installer: **3/70** on fresh VirusTotal reanalysis (previously 3/71).
  Microsoft: `Trojan:Win32/Wacatac.B!ml`; SecureAge: `Malicious`;
  Skyhigh (SWG): `BehavesLike.Win32.ObfuscatedPoly.vc`.
- Executable extracted from the installer: **0/69** on a new VirusTotal scan.
- Executable extracted from the portable ZIP: **0/70** on a new VirusTotal scan.
- All three executable artifacts are unsigned. Zero detections do not prove safety;
  the comparison supports vendor review of the installer, not a confirmed false positive.

| Artifact | SHA-256 | Report |
| --- | --- | --- |
| Installer, 10,521,717 bytes | `f68e37651a27c093f56174b288eaf20663db8dcff60500dfcc74acead43f0602` | [VirusTotal](https://www.virustotal.com/gui/file/f68e37651a27c093f56174b288eaf20663db8dcff60500dfcc74acead43f0602/detection) |
| Installed auricle.exe, 37,762,048 bytes | `33a3a742c5dd44ddd18f58a72ad05df888f839ad617860743fcab6b5f3c189c2` | [VirusTotal](https://www.virustotal.com/gui/file/33a3a742c5dd44ddd18f58a72ad05df888f839ad617860743fcab6b5f3c189c2/detection) |
| Portable auricle.exe, 41,431,552 bytes | `ac123ef06df918d7ee0acfa76d455398dc5479e4751c9a9d836f2ee80ee08b78` | [VirusTotal](https://www.virustotal.com/gui/file/ac123ef06df918d7ee0acfa76d455398dc5479e4751c9a9d836f2ee80ee08b78/detection) |

## Provenance and inspection

- Installer and portable ZIP SHA-256 values match the asset digests returned by the
  [GitHub release API](https://api.github.com/repos/californiaAlmonds/auricle/releases/tags/v0.1.4).
  Portable ZIP SHA-256: `5b638d02807f4cce94ae3f277fd2cf3a1cc3e73cb5efd61c008ac17131aa59fa`.
- [Release CI run](https://github.com/californiaAlmonds/auricle/actions/runs/34750210439)
  checked out `a52e7bd81dcecaba9f0e748f9874f475a21bc91e`.
  The current v0.1.4 tag is `624658fc6bb779622c5f9fcc88d5f2e0abac9f46`.
  Comparing these commits showed no changes under src/, ui/, Cargo.toml,
  Cargo.lock, build.rs, or packaging/inno/auricle.iss. This does not establish
  byte-for-byte reproducible builds or cryptographic build attestation.
- The installer contains one application executable. Inno Setup 6.7.0 was identified
  by innounp 2.71.1. The unpacker's archive was checked against Scoop's published
  SHA-256 `f2f037fdbc63de31248efae9ccb294398d160dc0cbad5f36d14c2f159e17bbf5`.
  The installer was extracted without executing it.
- The recovered installer script copies the executable, creates shortcuts and
  optionally launches Auricle. No custom Inno [Code] section was found.
  No yt-dlp, ffmpeg, Deno, or additional application DLL was bundled.
- The two application binaries differ by design: the portable build enables
  self-update, while the installer build uses `--no-default-features`.
- Windows Defender status inspection failed with provider error `0x80041013`.
  No successful local Defender scan or security-setting change is claimed.

## Behavior findings and limits

The installer sandbox's medium-severity "Unsigned Image Loaded Into LSASS Process"
Sigma event names `C:\wpsj5tf1\dll\RalDBm.dll`, loaded into
`C:\Windows\System32\lsass.exe`, with SHA-256
`0bf011593b5cff9f46909e5998786e30d98440bffe7251163570ca11cd38e07c`.
This is not a bundled installer file. Its origin and causal relationship to Auricle
remain unverified; it must not be described as proven harmless sandbox instrumentation.

A scoped source review found no explicit LSASS/RalDBm references or calls to
WriteProcessMemory, CreateRemoteThread, or VirtualAllocEx. This is not an exhaustive
audit of compiled dependencies. Add-on discovery, subprocess launches, theme-registry
reads, and network activity are present in the source and are expected behaviors.

Separate hardening opportunity: src/core/addons.rs downloads optional executables
from upstream HTTPS release URLs without verifying published hashes in its direct
download paths. Installed yt-dlp is automatically checked for updates at most weekly.
This is not established as the reason for the installer detections and was not changed
as part of this investigation.

## Vendor review status

- SecureAge: September 16 email confirms submission
  `20260915-041625322700/auricle-0.1.4-setup.exe` was successfully processed and false
  detections should clear within 72 hours. It warns that APEX results on VirusTotal
  may take longer and recommends reanalysis. Follow-up address supplied by the vendor:
  `secureaplus@secureage.com`.
- Microsoft: developer submission confirmed:
  [1cebfa1e-c00d-4f8f-87cd-638c7cf691df](https://www.microsoft.com/en-us/wdsi/submission/1cebfa1e-c00d-4f8f-87cd-638c7cf691df).
  Checked September 21: final determination **Not malware**; the analyst states that
  the files do not meet malware or potentially unwanted application criteria and
  "The detection has been removed." Cloud and client report no malware detected
  (client definition 1.459.218.0). The overall case status still says "In progress";
  this does not negate the explicit file determination and analyst response.
- Skyhigh: reporting address verified from
  [VirusTotal's vendor contacts](https://docs.virustotal.com/docs/false-positive-contacts):
  `GatewayAnti-Malware-SupportEscalations@SkyhighSecurity.com`. The review email was
  sent on 2026-09-15; Gmail confirmed "Message sent". It includes the public report,
  download and source links, hashes, scan comparisons, and unresolved sandbox event.
  A targeted mailbox search on September 21 found the sent request but no reply from
  the vendor's domain. No case number was found. The message copy remains staged at
  target/tmp/antivirus-0.1.4/skyhigh-review.eml.

## Signing readiness

Signing is not enabled. The maintainer reported SignPath rejection on September 21;
the rejection reason has not been reviewed here. No certificate has been obtained.
The existing commented workflow fragment is not a complete signing integration.

[SignPath application](https://signpath.org/apply) and [conditions](https://signpath.org/terms)
require maintainer identity, project reputation evidence, MFA, defined signing roles,
manual release approval, and personal acceptance of the foundation's conditions.
The maintainer must supply their name and accept these commitments directly.
Do not invent adoption figures or claim SignPath sponsorship before acceptance.

Application facts that can be reused:

- Project: Auricle; repository: https://github.com/californiaAlmonds/auricle
- Homepage: https://californiaalmonds.github.io/auricle/
- Download page: https://github.com/californiaAlmonds/auricle/releases
- Tagline: Native Windows music player built with Rust and Slint.
- Description: Auricle is an open-source Windows desktop music player for the
  YouTube Music catalogue, with search, a local library, queue management and
  playback controls. It uses a native interface and is licensed under GPLv3.
- Build system: GitHub Actions. Maintainer handle: californiaAlmonds.
- Reputation: early release; provide actual public release/download statistics and
  repository activity for assessment, without implying broad adoption.

Before production signing, publish a truthful privacy/code-signing policy covering
catalogue/media requests, thumbnails, release checks and add-on updates. Do not use
the blanket claim that all networking is explicitly user-requested without auditing
startup/background requests. Review eligibility and required user controls with SignPath.

Future integration must sign both application executables before packaging, create
the installer from its signed payload, sign the installer, and compute/publish hashes
only after signing. Verify signatures and timestamps before distribution. Never mutate
the existing 0.1.4 assets or hashes as a signing experiment. Signing establishes
publisher identity and integrity; it does not guarantee antivirus clearance.