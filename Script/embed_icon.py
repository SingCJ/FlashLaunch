import ctypes
import struct
import sys
import time
from pathlib import Path


RT_ICON = 3
RT_GROUP_ICON = 14
LANG_NEUTRAL = 0
RETRY_COUNT = 20
RETRY_DELAY_SECONDS = 0.5


def make_int_resource(value):
    return ctypes.c_void_p(value)


def parse_ico(path):
    data = path.read_bytes()
    if len(data) < 6:
        raise ValueError("Invalid ICO file")
    reserved, icon_type, count = struct.unpack_from("<HHH", data, 0)
    if reserved != 0 or icon_type != 1 or count == 0:
        raise ValueError("Invalid ICO header")

    entries = []
    offset = 6
    for index in range(count):
        entry = struct.unpack_from("<BBBBHHII", data, offset)
        width, height, colors, reserved, planes, bit_count, size, image_offset = entry
        image = data[image_offset : image_offset + size]
        if len(image) != size:
            raise ValueError(f"Invalid ICO image #{index + 1}")
        entries.append(
            {
                "width": width,
                "height": height,
                "colors": colors,
                "reserved": reserved,
                "planes": planes,
                "bit_count": bit_count,
                "size": size,
                "image": image,
                "resource_id": index + 1,
            }
        )
        offset += 16
    return entries


def build_group(entries):
    output = bytearray(struct.pack("<HHH", 0, 1, len(entries)))
    for entry in entries:
        output.extend(
            struct.pack(
                "<BBBBHHIH",
                entry["width"],
                entry["height"],
                entry["colors"],
                entry["reserved"],
                entry["planes"],
                entry["bit_count"],
                entry["size"],
                entry["resource_id"],
            )
        )
    return bytes(output)


def update_resource(handle, resource_type, resource_name, data):
    buffer = ctypes.create_string_buffer(data)
    ok = ctypes.windll.kernel32.UpdateResourceW(
        handle,
        make_int_resource(resource_type),
        make_int_resource(resource_name),
        LANG_NEUTRAL,
        buffer,
        len(data),
    )
    if not ok:
        raise ctypes.WinError()


def embed_icon(exe_path, ico_path):
    entries = parse_ico(ico_path)

    kernel32 = ctypes.windll.kernel32
    kernel32.BeginUpdateResourceW.argtypes = [ctypes.c_wchar_p, ctypes.c_int]
    kernel32.BeginUpdateResourceW.restype = ctypes.c_void_p
    kernel32.UpdateResourceW.argtypes = [
        ctypes.c_void_p,
        ctypes.c_void_p,
        ctypes.c_void_p,
        ctypes.c_ushort,
        ctypes.c_void_p,
        ctypes.c_uint,
    ]
    kernel32.UpdateResourceW.restype = ctypes.c_int
    kernel32.EndUpdateResourceW.argtypes = [ctypes.c_void_p, ctypes.c_int]
    kernel32.EndUpdateResourceW.restype = ctypes.c_int

    last_error = None
    for attempt in range(1, RETRY_COUNT + 1):
        handle = kernel32.BeginUpdateResourceW(str(exe_path), False)
        if not handle:
            last_error = ctypes.WinError()
            time.sleep(RETRY_DELAY_SECONDS)
            continue

        try:
            for entry in entries:
                update_resource(handle, RT_ICON, entry["resource_id"], entry["image"])
            update_resource(handle, RT_GROUP_ICON, 1, build_group(entries))
            if kernel32.EndUpdateResourceW(handle, False):
                return
            last_error = ctypes.WinError()
        except Exception as error:
            last_error = error
            kernel32.EndUpdateResourceW(handle, True)

        if attempt < RETRY_COUNT:
            time.sleep(RETRY_DELAY_SECONDS)

    if last_error is not None:
        raise last_error
    raise ctypes.WinError()


def main():
    if len(sys.argv) != 3:
        print("Usage: embed_icon.py <exe> <ico>", file=sys.stderr)
        return 2
    embed_icon(Path(sys.argv[1]), Path(sys.argv[2]))
    print(f"Embedded icon: {sys.argv[1]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
