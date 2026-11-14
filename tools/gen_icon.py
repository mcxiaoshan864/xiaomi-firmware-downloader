"""生成 App 图标：橙色渐变圆角方块 + 白色下载箭头（4x 超采样，多尺寸 ICO）。"""

from PIL import Image, ImageDraw, ImageFilter

S = 1024  # 超采样画布
BASE = S // 4  # 逻辑尺寸 256


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))


def rounded_mask(size, radius):
    mask = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(mask)
    d.rounded_rectangle((0, 0, size - 1, size - 1), radius=radius, fill=255)
    return mask


def build() -> Image.Image:
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))

    # 背景渐变：左上偏亮橙 -> 右下深橙
    top = (255, 160, 74)
    bottom = (255, 84, 0)
    grad = Image.new("RGB", (S, S))
    gd = ImageDraw.Draw(grad)
    for y in range(S):
        gd.line([(0, y), (S, y)], fill=lerp(top, bottom, y / (S - 1)))

    mask = rounded_mask(S, int(S * 0.225))
    img.paste(grad, (0, 0), mask)

    d = ImageDraw.Draw(img)

    # 顶部高光
    highlight = Image.new("L", (S, S), 0)
    hd = ImageDraw.Draw(highlight)
    hd.rounded_rectangle((int(S * 0.06), int(S * 0.05), int(S * 0.94), int(S * 0.52)),
                         radius=int(S * 0.2), fill=14)
    highlight = highlight.filter(ImageFilter.GaussianBlur(S * 0.03))
    highlight = Image.composite(highlight, Image.new("L", (S, S), 0), mask)
    white = Image.new("RGBA", (S, S), (255, 255, 255, 255))
    img = Image.alpha_composite(img, Image.composite(white, Image.new("RGBA", (S, S), (0, 0, 0, 0)), highlight))

    # 白色下载箭头（简洁版：粗箭头 + 底线）
    d = ImageDraw.Draw(img)
    cx = S // 2
    d.rounded_rectangle((cx - 70, int(S * 0.19), cx + 70, int(S * 0.45)), radius=70, fill="white")
    d.polygon([(cx - 212, int(S * 0.40)), (cx + 212, int(S * 0.40)), (cx, int(S * 0.635))], fill="white")
    d.rounded_rectangle((int(S * 0.255), int(S * 0.725), int(S * 0.745), int(S * 0.805)),
                        radius=int(S * 0.04), fill="white")

    return img.resize((BASE, BASE), Image.LANCZOS)


def main():
    icon = build()
    icon.save("icon_preview_256.png")
    icon.resize((64, 64), Image.LANCZOS).save("icon_preview_64.png")
    icon.resize((32, 32), Image.LANCZOS).save("icon_preview_32.png")
    sizes = [(16, 16), (20, 20), (24, 24), (32, 32), (40, 40), (48, 48),
             (64, 64), (128, 128), (256, 256)]
    icon.save("app.ico", sizes=sizes)
    print("icon generated", icon.size)


if __name__ == "__main__":
    main()
