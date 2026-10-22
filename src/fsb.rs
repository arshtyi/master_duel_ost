use std::io::{Cursor, Write};

use anyhow::{Context, Result, ensure};
use byteorder::{LittleEndian, ReadBytesExt};
use lewton::{
    audio::get_decoded_sample_count,
    header::{read_header_ident, read_header_setup},
};
use ogg::{PacketWriteEndInfo, PacketWriter};

use crate::unity::Clip;

pub fn write_ogg(data: &[u8], clip: &Clip, output: impl Write) -> Result<()> {
    ensure!(data.starts_with(b"FSB5"), "expected FSB5 audio");
    let mut reader = Cursor::new(data);
    reader.set_position(4);
    let version = reader.read_u32::<LittleEndian>()?;
    ensure!(version <= 1, "unsupported FSB5 version {version}");
    ensure!(
        reader.read_u32::<LittleEndian>()? == 1,
        "expected one FSB5 subsound"
    );
    let headers_size = reader.read_u32::<LittleEndian>()? as usize;
    let names_size = reader.read_u32::<LittleEndian>()? as usize;
    let audio_size = reader.read_u32::<LittleEndian>()? as usize;
    ensure!(
        reader.read_u32::<LittleEndian>()? == 15,
        "expected Vorbis music"
    );
    let header_size = if version == 0 { 64 } else { 60 };
    reader.set_position(header_size as u64);
    let mode = reader.read_u64::<LittleEndian>()?;
    let samples = mode >> 34;
    let mut more = mode & 1 != 0;
    let mut crc = None;
    while more {
        let chunk = reader.read_u32::<LittleEndian>()?;
        more = chunk & 1 != 0;
        let end = reader.position() + u64::from((chunk >> 1) & 0xff_ffff);
        ensure!(
            end <= (header_size + headers_size) as u64,
            "truncated FSB5 metadata"
        );
        if chunk >> 25 == 11 {
            crc = Some(reader.read_u32::<LittleEndian>()?);
        }
        reader.set_position(end);
    }
    let crc = crc.context("FSB5 Vorbis setup CRC is missing")?;
    let setup = setup_header(crc).with_context(|| format!("unknown Vorbis setup {crc:08x}"))?;
    let mut ident = b"\x01vorbis\0\0\0\0".to_vec();
    ident.push(clip.channels);
    ident.extend(clip.sample_rate.to_le_bytes());
    ident.extend([0; 12]);
    ident.extend([0xb8, 1]);
    let id = read_header_ident(&ident)?;
    let setup_info = read_header_setup(setup, clip.channels, (8, 11))?;
    let start = header_size + headers_size + names_size;
    let audio = data
        .get(start..start + audio_size)
        .context("truncated FSB5 audio")?;
    let mut packets = Vec::new();
    let mut reader = Cursor::new(audio);
    reader.set_position(((mode >> 7) & 0x7ff_ffff) * 32);
    while (reader.position() as usize) + 2 <= audio.len() {
        let size = reader.read_u16::<LittleEndian>()? as usize;
        if size == 0 || size == 0xffff {
            break;
        }
        let start = reader.position() as usize;
        packets.push(
            audio
                .get(start..start + size)
                .context("truncated Vorbis packet")?,
        );
        reader.set_position((start + size) as u64);
    }
    ensure!(!packets.is_empty(), "empty Vorbis stream");
    let mut writer = PacketWriter::new(output);
    writer.write_packet(ident, 1, PacketWriteEndInfo::EndPage, 0)?;
    writer.write_packet(
        b"\x03vorbis\0\0\0\0\0\0\0\0\x01".to_vec(),
        1,
        PacketWriteEndInfo::NormalPacket,
        0,
    )?;
    writer.write_packet(setup.to_vec(), 1, PacketWriteEndInfo::EndPage, 0)?;
    let mut position = 0;
    for (index, packet) in packets.iter().enumerate() {
        if index > 0 {
            position += get_decoded_sample_count(&id, &setup_info, packet)? as u64;
        }
        let last = index + 1 == packets.len();
        let end = if last {
            PacketWriteEndInfo::EndStream
        } else {
            PacketWriteEndInfo::NormalPacket
        };
        writer.write_packet(
            packet.to_vec(),
            1,
            end,
            if last { samples } else { position },
        )?;
    }
    writer.into_inner().flush()?;
    Ok(())
}

fn setup_header(crc: u32) -> Option<&'static [u8]> {
    let table = include_bytes!("../data/fsb_vorbis_headers.bin");
    let count = u32::from_le_bytes(table[8..12].try_into().ok()?) as usize;
    for record in table[12..12 + count * 12].as_chunks::<12>().0 {
        if u32::from_le_bytes(record[..4].try_into().ok()?) == crc {
            let offset = u32::from_le_bytes(record[4..8].try_into().ok()?) as usize;
            let size = u32::from_le_bytes(record[8..].try_into().ok()?) as usize;
            return table.get(offset..offset + size);
        }
    }
    None
}
