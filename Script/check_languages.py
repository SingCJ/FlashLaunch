"""Check shipped translations without launching the UI or contacting a service."""
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
LOCALIZED_CALL = re.compile(
    r'\blocalized(?:_active)?(?:_format[123])?\(\s*(?:[^,()\n]+,\s*)?"((?:\\.|[^"\\])*)"',
    re.S,
)


def unescape(value):
    escapes = {'n': '\n', 'r': '\r', 't': '\t', '\\': '\\', '=': '=', '"': '"'}
    return re.sub(r'\\([nrt\\="])', lambda match: escapes[match[1]], value)


def read_pack(path):
    values = {}
    for number, line in enumerate(path.read_text(encoding='utf-8-sig').splitlines(), 1):
        line = line.strip()
        if not line or line.startswith(('#', ';', '[')):
            continue
        match = re.search(r'(?<!\\)(?:\\\\)*=', line)
        if not match:
            raise ValueError(f'{path.name}:{number}: missing separator')
        index = match.end() - 1
        key, value = unescape(line[:index].strip()), unescape(line[index + 1:].strip())
        if key in values or not key or not value:
            raise ValueError(f'{path.name}:{number}: duplicate/empty key or value: {key!r}')
        values[key] = value
    return values


def check():
    source_keys = set()
    for path in (ROOT / 'SOURCE').rglob('*.rs'):
        source_keys.update(unescape(match[1]) for match in LOCALIZED_CALL.finditer(path.read_text(encoding='utf-8-sig')))

    packs = {path.stem: read_pack(path) for path in (ROOT / 'Languages').glob('*.ini')}
    if not packs:
        raise ValueError('No language packs found')
    reference_keys = set(packs['vi']) - {'id', 'name'}
    missing = source_keys - reference_keys
    if missing:
        raise ValueError(f'Untranslated source literals: {sorted(missing)}')

    for language, pack in sorted(packs.items()):
        if pack.get('id') != language or not pack.get('name'):
            raise ValueError(f'{language}: invalid metadata')
        if set(pack) - {'id', 'name'} != reference_keys:
            raise ValueError(f'{language}: inconsistent translation keys')
        english_identical = [key for key in reference_keys if pack[key] == key]
        if len(english_identical) > max(20, len(reference_keys) // 10):
            raise ValueError(
                f'{language}: too many untranslated values '
                f'({len(english_identical)}/{len(reference_keys)})'
            )
        for key in reference_keys:
            value = pack[key]
            placeholders = lambda text: sorted(re.findall(r'\{[^{}]*\}', text))
            if placeholders(key) != placeholders(value):
                raise ValueError(f'{language}: changed placeholders: {key!r}')
            if key.count('\n') != value.count('\n') or key.count('\r') != value.count('\r'):
                raise ValueError(f'{language}: changed line breaks: {key!r}')
            for syntax in ('CONFIG\\scoring.ini', 'recent_items.txt', '/c', '+keyword', '-keyword', '+mp3', '<<<'):
                if syntax in key and syntax not in value:
                    raise ValueError(f'{language}: missing {syntax!r}: {key!r}')
            if re.search(r'[ZXQ]{2,}\d{3}[QXZ]{2,}', value):
                raise ValueError(f'{language}: unfinished translation marker: {key!r}')
        print(f'{language}: {len(reference_keys)} translations OK')
    print(f'Covered {len(source_keys)} literal localization calls; English is built in.')


if __name__ == '__main__':
    check()
