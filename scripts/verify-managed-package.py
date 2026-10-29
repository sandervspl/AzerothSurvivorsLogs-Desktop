"""Inspect actual Velopack output; never accept an addon payload in the installer."""
from pathlib import Path
import hashlib
import json
import os
import zipfile

root = Path(os.environ['AZS_MANAGED_PACKAGE_DIR']).resolve()
feed = json.loads((root / 'releases.stable.json').read_text(encoding='utf-8-sig'))
evidence = []
signature_files = []
for asset in feed['Assets']:
    if asset['Type'] != 'Full':
        raise SystemExit('Only full authenticated packages are supported')
    package = root / asset['FileName']
    if package.parent != root:
        raise SystemExit('Unsafe package filename')
    digest = hashlib.file_digest(package.open('rb'), 'sha256').hexdigest()
    if digest.lower() != asset['SHA256'].lower() or package.stat().st_size != asset['Size']:
        raise SystemExit('Managed package hash/size mismatch')
    with zipfile.ZipFile(package) as archive:
        names = archive.namelist()
        if archive.testzip() is not None:
            raise SystemExit('Corrupt managed package')
        runtime = [name for name in names if name.startswith('lib/app/') and not name.endswith('/')]
        title = 'Azeroth Survivors Uploader Development' if os.environ.get('AZS_MANAGED_PACKAGE_DEVELOPMENT') == '1' else 'Azeroth Survivors Uploader'
        expected = [title + '_ExecutionStub.exe', 'Squirrel.exe', 'azs-update-guard.exe', 'azs-uploader.exe', 'sq.version']
        if sorted(runtime) != ['lib/app/' + name for name in expected]:
            raise SystemExit('Unexpected application payload; addon files must be downloaded separately')
        if any(name.lower().endswith(('.lua', '.toc')) or 'azerothsurvivorsrecorder/' in name.lower() for name in names):
            raise SystemExit('Addon payload is forbidden in a desktop package')
        if os.environ.get('AZS_MANAGED_PACKAGE_DEVELOPMENT') != '1':
            signature_dir = root / 'signature-check'
            signature_dir.mkdir(exist_ok=True)
            for name in runtime:
                if name.endswith('.exe'):
                    target = signature_dir / Path(name).name
                    target.write_bytes(archive.read(name))
                    signature_files.append(str(target))
    evidence.append({'package': package.name, 'sha256': digest, 'bytes': package.stat().st_size, 'runtimeFiles': runtime})
if not evidence or not list(root.glob('*Setup.exe')):
    raise SystemExit('Missing installer or update package')
signature_files.extend(str(p) for p in root.glob('*Setup.exe'))
(root / 'managed-package-verification.json').write_text(json.dumps({'development': os.environ.get('AZS_MANAGED_PACKAGE_DEVELOPMENT') == '1', 'packages': evidence, 'signatureFiles': signature_files}, indent=2), encoding='utf-8')
print(json.dumps({'verifiedPackages': len(evidence), 'addonBundled': False}))
