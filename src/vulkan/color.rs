use anyhow::{Result, ensure};
use ffmpeg_sys_next as ff;

#[derive(Clone, Copy)]
pub struct Color {
    pub luma_offset: f32,
    pub luma_scale: f32,
    pub chroma_scale: f32,
    pub coefficients: [f32; 4],
}

impl Color {
    pub fn from_frame(frame: &ff::AVFrame) -> Result<Self> {
        ensure!(
            matches!(
                frame.color_trc,
                ff::AVColorTransferCharacteristic::AVCOL_TRC_UNSPECIFIED
                    | ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709
                    | ff::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE170M
                    | ff::AVColorTransferCharacteristic::AVCOL_TRC_IEC61966_2_1
                    | ff::AVColorTransferCharacteristic::AVCOL_TRC_BT2020_10
            ),
            "transfer function {:?} requires --backend mpv",
            frame.color_trc,
        );
        ensure!(
            matches!(
                frame.color_primaries,
                ff::AVColorPrimaries::AVCOL_PRI_UNSPECIFIED
                    | ff::AVColorPrimaries::AVCOL_PRI_BT709
                    | ff::AVColorPrimaries::AVCOL_PRI_BT470BG
                    | ff::AVColorPrimaries::AVCOL_PRI_SMPTE170M
            ),
            "color primaries {:?} require --backend mpv",
            frame.color_primaries,
        );

        let (kr, kb) = match frame.colorspace {
            ff::AVColorSpace::AVCOL_SPC_BT709 => (0.2126, 0.0722),
            ff::AVColorSpace::AVCOL_SPC_BT470BG | ff::AVColorSpace::AVCOL_SPC_SMPTE170M => {
                (0.299, 0.114)
            }
            ff::AVColorSpace::AVCOL_SPC_UNSPECIFIED if frame.height >= 720 => (0.2126, 0.0722),
            ff::AVColorSpace::AVCOL_SPC_UNSPECIFIED => (0.299, 0.114),
            other => anyhow::bail!("color matrix {other:?} requires --backend mpv"),
        };
        let kg = 1.0 - kr - kb;
        let r = 2.0 * (1.0 - kr);
        let b = 2.0 * (1.0 - kb);
        let full = frame.color_range == ff::AVColorRange::AVCOL_RANGE_JPEG;
        Ok(Self {
            luma_offset: if full { 0.0 } else { 16.0 / 255.0 },
            luma_scale: if full { 1.0 } else { 255.0 / 219.0 },
            chroma_scale: if full { 1.0 } else { 255.0 / 224.0 },
            coefficients: [r, kb * b / kg, kr * r / kg, b],
        })
    }
}
