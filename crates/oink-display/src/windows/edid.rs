//! EDID 解析（纯函数，便于单元测试）：厂商、型号、名字、物理尺寸。
//!
//! 物理尺寸优先取第一个「详细时序描述符」（DTD）里的毫米值；它与基本参数里的厘米值相差过大时
//! （有的显示器在 DTD 里填宽高比或错误值），退回厘米值。两者都不可信时返回 `None`。
//! 尺寸是显示器**原生方向**的，旋转由调用方处理。

/// 从 EDID 得到的信息。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct EdidInfo {
    /// PNP 厂商 ID，如 `GSM`、`DEL`。
    pub manufacturer: String,
    pub product: u16,
    pub serial: u32,
    /// 显示器名字描述符（0xFC）。
    pub name: Option<String>,
    /// Native-orientation visible width and height in metres.
    pub size_meters: Option<(f64, f64)>,
    /// Basic and DTD measurements converted to metres for numeric diagnostics.
    pub basic_meters: Option<(f64, f64)>,
    pub dtd_meters: Option<(f64, f64)>,
}

const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
/// DTD 毫米值与厘米值允许的最大差：`max(20 mm, 6%)`。厘米值是取整（常见向上取整）后的，
/// 实测显示器上两者常差 1–2 cm；宽高比占位值（如 16×9）与厘米值相差远超这个范围。
fn dtd_tolerance_meters(cm_value: f64) -> f64 {
    (cm_value * 0.06).max(0.020)
}

pub(crate) fn parse(edid: &[u8]) -> Option<EdidInfo> {
    if edid.len() < 128 || edid[..8] != HEADER {
        return None;
    }
    let m = u16::from_be_bytes([edid[8], edid[9]]);
    let letter = |v: u16| {
        let v = (v & 0x1f) as u8;
        if (1..=26).contains(&v) {
            (b'A' + v - 1) as char
        } else {
            '?'
        }
    };
    let manufacturer: String = [letter(m >> 10), letter(m >> 5), letter(m)].iter().collect();
    let product = u16::from_le_bytes([edid[10], edid[11]]);
    let serial = u32::from_le_bytes([edid[12], edid[13], edid[14], edid[15]]);

    let cm = (edid[21] > 0 && edid[22] > 0).then(|| (f64::from(edid[21]) / 100.0, f64::from(edid[22]) / 100.0));
    let mut dtd = None;
    let mut name = None;
    for i in 0..4 {
        let d = &edid[54 + i * 18..54 + (i + 1) * 18];
        if d[0] != 0 || d[1] != 0 {
            // 详细时序描述符
            if dtd.is_none() {
                let w = u16::from(d[12]) | (u16::from(d[14] >> 4) << 8);
                let h = u16::from(d[13]) | (u16::from(d[14] & 0x0f) << 8);
                if w > 0 && h > 0 {
                    dtd = Some((f64::from(w) / 1000.0, f64::from(h) / 1000.0));
                }
            }
        } else if d[3] == 0xFC && name.is_none() {
            name = descriptor_text(&d[5..18]);
        }
    }
    let size_meters = match (dtd, cm) {
        (Some(d), Some(c)) => {
            if (d.0 - c.0).abs() <= dtd_tolerance_meters(c.0) && (d.1 - c.1).abs() <= dtd_tolerance_meters(c.1) {
                Some(d)
            } else {
                Some(c)
            }
        }
        // 只有 DTD：排除「16×9」这类宽高比占位值
        (Some(d), None) => (d.0 >= 0.050 && d.1 >= 0.030).then_some(d),
        (None, c) => c,
    };
    Some(EdidInfo {
        manufacturer,
        product,
        serial,
        name,
        size_meters,
        basic_meters: cm,
        dtd_meters: dtd,
    })
}

fn descriptor_text(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0x0A).unwrap_or(bytes.len());
    let s: String = bytes[..end]
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                ' '
            }
        })
        .collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// 按旋转角度把原生方向的尺寸转成当前方向。
pub(crate) fn rotate(size: (f64, f64), rotation_deg: u16) -> (f64, f64) {
    if rotation_deg == 90 || rotation_deg == 270 {
        (size.1, size.0)
    } else {
        size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个最小 EDID：厂商 GSM，型号 0x5B09，厘米 (cm_w, cm_h)，DTD 毫米 (w, h)，名字 "LG HDR 4K"。
    fn sample(cm: (u8, u8), dtd_units: (u16, u16)) -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[..8].copy_from_slice(&HEADER);
        // GSM: G=7, S=19, M=13
        let m: u16 = (7 << 10) | (19 << 5) | 13;
        e[8..10].copy_from_slice(&m.to_be_bytes());
        e[10..12].copy_from_slice(&0x5B09u16.to_le_bytes());
        e[12..16].copy_from_slice(&0x0001_E240u32.to_le_bytes());
        e[21] = cm.0;
        e[22] = cm.1;
        // DTD at 54
        let d = &mut e[54..72];
        d[0] = 0x08;
        d[1] = 0xE8; // pixel clock 594 MHz
        d[12] = (dtd_units.0 & 0xff) as u8;
        d[13] = (dtd_units.1 & 0xff) as u8;
        d[14] = (((dtd_units.0 >> 8) as u8) << 4) | ((dtd_units.1 >> 8) as u8 & 0x0f);
        // name descriptor at 72
        let n = &mut e[72..90];
        n[3] = 0xFC;
        let text = b"LG HDR 4K\n   ";
        n[5..18].copy_from_slice(text);
        e
    }

    #[test]
    fn prefers_dtd_millimetres() {
        let info = parse(&sample((63, 36), (621, 341))).unwrap();
        assert_eq!(info.manufacturer, "GSM");
        assert_eq!(info.product, 0x5B09);
        assert_eq!(info.serial, 123_456);
        assert_eq!(info.name.as_deref(), Some("LG HDR 4K"));
        assert_eq!(info.size_meters, Some((0.621, 0.341)));
    }

    #[test]
    fn falls_back_to_cm_when_dtd_is_aspect_ratio() {
        let info = parse(&sample((63, 36), (16, 9))).unwrap();
        assert_eq!(info.size_meters, Some((0.630, 0.360)));
    }

    #[test]
    fn dtd_only_rejects_placeholders() {
        assert_eq!(parse(&sample((0, 0), (16, 9))).unwrap().size_meters, None);
        assert_eq!(
            parse(&sample((0, 0), (344, 194))).unwrap().size_meters,
            Some((0.344, 0.194))
        );
    }

    #[test]
    fn cm_zero_means_unknown() {
        // 投影仪：厘米值为 0，DTD 也没有尺寸
        assert_eq!(parse(&sample((0, 0), (0, 0))).unwrap().size_meters, None);
    }

    #[test]
    fn rejects_bad_header_and_short_input() {
        assert!(parse(&[0u8; 128]).is_none());
        assert!(parse(&sample((63, 36), (621, 341))[..100]).is_none());
    }

    #[test]
    fn dtd_upper_nibbles() {
        // 1210 × 680 mm（> 255，需要高 4 位）
        let info = parse(&sample((121, 68), (1210, 680))).unwrap();
        assert_eq!(info.size_meters, Some((1.210, 0.680)));
    }

    #[test]
    fn rotation_swaps() {
        assert_eq!(rotate((600.0, 340.0), 90), (340.0, 600.0));
        assert_eq!(rotate((600.0, 340.0), 180), (600.0, 340.0));
        assert_eq!(rotate((600.0, 340.0), 270), (340.0, 600.0));
    }
}
