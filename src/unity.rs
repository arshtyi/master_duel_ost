use std::{
    fs::File,
    io::{BufReader, Cursor, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use byteorder::{BigEndian, LittleEndian, ReadBytesExt};

struct Block {
    offset: u64,
    start: u64,
    size: u32,
    compressed_size: u32,
    flags: u16,
}

#[derive(Clone)]
struct Node {
    offset: u64,
    size: u64,
    flags: u32,
    path: String,
}

pub struct Clip {
    pub name: String,
    pub channels: u8,
    pub sample_rate: u32,
    pub resource: Option<PathBuf>,
    offset: u64,
    size: u64,
}

pub struct Bundle {
    file: BufReader<File>,
    directory: PathBuf,
    blocks: Vec<Block>,
    nodes: Vec<Node>,
    cached: Option<(usize, Vec<u8>)>,
    pub bytes_read: u64,
}

impl Bundle {
    pub fn open(path: &Path) -> Result<Option<Self>> {
        let mut file = BufReader::new(File::open(path)?);
        let mut signature = [0; 8];
        if file.read(&mut signature)? != 8 || &signature != b"UnityFS\0" {
            return Ok(None);
        }
        let version = file.read_u32::<BigEndian>()?;
        ensure!(
            (7..=8).contains(&version),
            "unsupported UnityFS version {version}"
        );
        cstring(&mut file)?;
        cstring(&mut file)?;
        let size = file.read_u64::<BigEndian>()?;
        let compressed_size = file.read_u32::<BigEndian>()?;
        let info_size = file.read_u32::<BigEndian>()?;
        let flags = file.read_u32::<BigEndian>()?;
        let header_end = file.stream_position()?.next_multiple_of(16);
        let info_offset = if flags & 0x80 != 0 {
            size - u64::from(compressed_size)
        } else {
            header_end
        };
        file.seek(SeekFrom::Start(info_offset))?;
        let compressed = read_bytes(&mut file, compressed_size as usize)?;
        let mut info = Cursor::new(decompress(compressed, info_size, flags as u16)?);
        let mut offset = if flags & 0x80 != 0 {
            header_end
        } else {
            header_end + u64::from(compressed_size)
        };
        if flags & 0x200 != 0 {
            offset = offset.next_multiple_of(16);
        }
        info.set_position(16);
        let mut blocks = Vec::new();
        let mut start = 0;
        for _ in 0..info.read_u32::<BigEndian>()? {
            let size = info.read_u32::<BigEndian>()?;
            let compressed_size = info.read_u32::<BigEndian>()?;
            let flags = info.read_u16::<BigEndian>()?;
            blocks.push(Block {
                offset,
                start,
                size,
                compressed_size,
                flags,
            });
            offset += u64::from(compressed_size);
            start += u64::from(size);
        }
        let mut nodes = Vec::new();
        for _ in 0..info.read_u32::<BigEndian>()? {
            nodes.push(Node {
                offset: info.read_u64::<BigEndian>()?,
                size: info.read_u64::<BigEndian>()?,
                flags: info.read_u32::<BigEndian>()?,
                path: cstring(&mut info)?,
            });
        }
        Ok(Some(Self {
            file,
            directory: path.parent().unwrap().to_path_buf(),
            blocks,
            nodes,
            cached: None,
            bytes_read: header_end + u64::from(compressed_size),
        }))
    }

    fn range(&mut self, offset: u64, size: u64) -> Result<Vec<u8>> {
        let end = offset.checked_add(size).context("invalid bundle range")?;
        let mut result = Vec::with_capacity(usize::try_from(size)?);
        for (index, block) in self.blocks.iter().enumerate() {
            let from = offset.max(block.start);
            let to = end.min(block.start + u64::from(block.size));
            if from >= to {
                continue;
            }
            let start = (from - block.start) as usize;
            let len = (to - from) as usize;
            if block.flags & 0x3f == 0 {
                self.file
                    .seek(SeekFrom::Start(block.offset + start as u64))?;
                result.extend(read_bytes(&mut self.file, len)?);
                self.bytes_read += len as u64;
            } else {
                if self
                    .cached
                    .as_ref()
                    .is_none_or(|(cached, bytes)| *cached != index || bytes.len() < start + len)
                {
                    self.file.seek(SeekFrom::Start(block.offset))?;
                    let decoded = if block.flags & 0x3f == 1 {
                        let input = (&mut self.file).take(u64::from(block.compressed_size));
                        let mut decoder = lzma_reader(input, block.size)?;
                        read_bytes(&mut decoder, start + len)?
                    } else {
                        let bytes = read_bytes(&mut self.file, block.compressed_size as usize)?;
                        decompress(bytes, block.size, block.flags)?
                    };
                    self.bytes_read += self.file.stream_position()? - block.offset;
                    self.cached = Some((index, decoded));
                }
                result.extend_from_slice(&self.cached.as_ref().unwrap().1[start..start + len]);
            }
        }
        ensure!(
            result.len() as u64 == size,
            "bundle range extends past data"
        );
        Ok(result)
    }

    pub fn music(&mut self) -> Result<Vec<Clip>> {
        let mut clips = Vec::new();
        for node in self
            .nodes
            .clone()
            .into_iter()
            .filter(|node| node.flags & 4 != 0)
        {
            let header = self.range(node.offset, 48.min(node.size))?;
            let mut reader = Reader::new(&header, false);
            let mut metadata_size = reader.u32()?;
            reader.skip(4)?;
            let version = reader.u32()?;
            ensure!(
                (17..=22).contains(&version),
                "unsupported serialized file version {version}"
            );
            let mut data_offset = u64::from(reader.u32()?);
            let little = reader.u8()? == 0;
            reader.skip(3)?;
            if version >= 22 {
                metadata_size = reader.u32()?;
                reader.skip(8)?;
                data_offset = reader.u64()?;
                reader.skip(8)?;
            }
            let header_size = reader.position();
            let metadata = self.range(node.offset + header_size, u64::from(metadata_size))?;
            let mut reader = Reader::new(&metadata, little);
            cstring(&mut reader.data)?;
            reader.skip(4)?;
            let type_tree = reader.u8()? != 0;
            let mut types = Vec::new();
            for _ in 0..reader.u32()? {
                let class = reader.u32()?;
                reader.skip(3)?;
                if class == 114 {
                    reader.skip(16)?;
                }
                reader.skip(16)?;
                if type_tree {
                    let nodes = u64::from(reader.u32()?);
                    let strings = u64::from(reader.u32()?);
                    reader.skip(nodes * if version >= 19 { 32 } else { 24 } + strings)?;
                    if version >= 21 {
                        let dependencies = reader.u32()?;
                        reader.skip(u64::from(dependencies) * 4)?;
                    }
                }
                types.push(class);
            }
            if !types.contains(&83) {
                continue;
            }
            let mut objects = Vec::new();
            for _ in 0..reader.u32()? {
                reader.align()?;
                reader.skip(8)?;
                let start = if version >= 22 {
                    reader.u64()?
                } else {
                    u64::from(reader.u32()?)
                };
                let size = reader.u32()?;
                let type_id = reader.u32()? as usize;
                if *types.get(type_id).context("invalid object type")? == 83 {
                    objects.push((node.offset + data_offset + start, size));
                }
            }
            for (start, size) in objects {
                let object = self.range(start, u64::from(size))?;
                let mut reader = Reader::new(&object, little);
                let name = reader.string()?;
                if !name.starts_with("BGM_") {
                    continue;
                }
                ensure!(!name.contains(['/', '\\']), "invalid audio name: {name}");
                reader.skip(4)?;
                let channels = u8::try_from(reader.u32()?)?;
                let sample_rate = reader.u32()?;
                reader.skip(8)?;
                reader.skip(1)?;
                reader.align()?;
                reader.skip(7)?;
                reader.align()?;
                let source = reader.string()?;
                let offset = reader.u64()?;
                let size = reader.u64()?;
                reader.skip(4)?;
                let (offset, resource) = if source.is_empty() {
                    (start + reader.position(), None)
                } else {
                    let resource = source.rsplit(['/', '\\']).next().unwrap();
                    match self.nodes.iter().find(|node| node.path == resource) {
                        Some(node) => (node.offset + offset, None),
                        None => (offset, Some(self.directory.join(resource))),
                    }
                };
                clips.push(Clip {
                    name,
                    channels,
                    sample_rate,
                    resource,
                    offset,
                    size,
                });
            }
        }
        Ok(clips)
    }

    pub fn audio(&mut self, clip: &Clip) -> Result<Vec<u8>> {
        if let Some(path) = &clip.resource {
            let mut file = File::open(path)
                .with_context(|| format!("missing audio resource {}", path.display()))?;
            file.seek(SeekFrom::Start(clip.offset))?;
            let bytes = read_bytes(&mut file, usize::try_from(clip.size)?)?;
            self.bytes_read += clip.size;
            return Ok(bytes);
        }
        self.range(clip.offset, clip.size)
    }
}

fn decompress(bytes: Vec<u8>, size: u32, flags: u16) -> Result<Vec<u8>> {
    match flags & 0x3f {
        0 => {
            ensure!(
                bytes.len() == size as usize,
                "invalid uncompressed block size"
            );
            Ok(bytes)
        }
        2 | 3 => Ok(lz4_flex::block::decompress(&bytes, size as usize)?),
        1 => read_bytes(&mut lzma_reader(bytes.as_slice(), size)?, size as usize),
        compression => bail!("unsupported UnityFS compression {compression}"),
    }
}

fn lzma_reader(mut input: impl Read, size: u32) -> Result<lzma_rust2::LzmaReader<impl Read>> {
    let props = input.read_u8()?;
    let dictionary = input.read_u32::<LittleEndian>()?;
    Ok(lzma_rust2::LzmaReader::new_with_props(
        input,
        u64::from(size),
        props,
        dictionary,
        None,
    )?)
}

fn read_bytes(reader: &mut impl Read, size: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn cstring(reader: &mut impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    loop {
        let byte = reader.read_u8()?;
        if byte == 0 {
            break;
        }
        bytes.push(byte);
    }
    Ok(String::from_utf8(bytes)?)
}

struct Reader<'a> {
    data: Cursor<&'a [u8]>,
    little: bool,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8], little: bool) -> Self {
        Self {
            data: Cursor::new(data),
            little,
        }
    }
    fn position(&self) -> u64 {
        self.data.position()
    }
    fn skip(&mut self, size: u64) -> Result<()> {
        let end = self
            .position()
            .checked_add(size)
            .context("invalid metadata range")?;
        ensure!(
            end <= self.data.get_ref().len() as u64,
            "truncated metadata"
        );
        self.data.set_position(end);
        Ok(())
    }
    fn align(&mut self) -> Result<()> {
        self.skip(self.position().next_multiple_of(4) - self.position())
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.data.read_u8()?)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(if self.little {
            self.data.read_u32::<LittleEndian>()?
        } else {
            self.data.read_u32::<BigEndian>()?
        })
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(if self.little {
            self.data.read_u64::<LittleEndian>()?
        } else {
            self.data.read_u64::<BigEndian>()?
        })
    }
    fn string(&mut self) -> Result<String> {
        let size = self.u32()? as usize;
        let value = String::from_utf8(read_bytes(&mut self.data, size)?)?;
        self.align()?;
        Ok(value)
    }
}
