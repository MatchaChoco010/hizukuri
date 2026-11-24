//! Checked handles.
//!
//! A handle is a plain 52-bit integer: `kind | device epoch | generation | index`.
//! It carries no pointer. Any integer can be passed where a handle is expected;
//! a value that does not name a live object of the right kind yields an error.
//!
//! 52 bits, because script languages whose only number type is an IEEE double
//! (Luau, JavaScript) represent integers exactly only up to 2^53.

use crate::error::{ErrorCode, GfxError, Result};

const INDEX_BITS: u32 = 20;
const GENERATION_BITS: u32 = 20;
const EPOCH_BITS: u32 = 8;
const INDEX_MASK: u64 = (1 << INDEX_BITS) - 1;
const GENERATION_MASK: u64 = (1 << GENERATION_BITS) - 1;
const EPOCH_MASK: u64 = (1 << EPOCH_BITS) - 1;
const KIND_MASK: u64 = 0xf;
const MAX_GENERATION: u32 = GENERATION_MASK as u32;

/// What a handle refers to.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    Buffer = 1,
    Image = 2,
    Shader = 3,
    RenderPipeline = 4,
    ComputePipeline = 5,
    /// A slot of the resource descriptor heap.
    Slot = 6,
    /// A slot of the sampler descriptor heap.
    Sampler = 7,
}

impl Kind {
    fn from_bits(bits: u64) -> Option<Self> {
        Some(match bits {
            1 => Self::Buffer,
            2 => Self::Image,
            3 => Self::Shader,
            4 => Self::RenderPipeline,
            5 => Self::ComputePipeline,
            6 => Self::Slot,
            7 => Self::Sampler,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Buffer => "buffer",
            Self::Image => "image",
            Self::Shader => "shader",
            Self::RenderPipeline => "render pipeline",
            Self::ComputePipeline => "compute pipeline",
            Self::Slot => "heap slot",
            Self::Sampler => "sampler",
        }
    }
}

/// A checked reference to a GPU object. `Handle::NULL` (0) never names anything.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct Handle(u64);

impl Handle {
    pub const NULL: Self = Self(0);

    fn new(kind: Kind, epoch: u8, generation: u32, index: u32) -> Self {
        Self(
            ((kind as u64) << (INDEX_BITS + GENERATION_BITS + EPOCH_BITS))
                | (u64::from(epoch) << (INDEX_BITS + GENERATION_BITS))
                | (u64::from(generation) << INDEX_BITS)
                | u64::from(index),
        )
    }

    /// The handle as an integer a script can hold. Always non-negative.
    pub fn to_bits(self) -> i64 {
        self.0 as i64
    }

    /// Reinterprets an integer coming from a script. Never fails; a bogus value
    /// is rejected when it is used.
    pub fn from_bits(bits: i64) -> Self {
        Self(bits as u64)
    }

    pub fn is_null(self) -> bool {
        self.0 == 0
    }

    pub fn kind(self) -> Option<Kind> {
        Kind::from_bits((self.0 >> (INDEX_BITS + GENERATION_BITS + EPOCH_BITS)) & KIND_MASK)
    }

    /// Position in the pool. For heap slots this is the descriptor index.
    pub fn index(self) -> u32 {
        (self.0 & INDEX_MASK) as u32
    }

    fn generation(self) -> u32 {
        ((self.0 >> INDEX_BITS) & GENERATION_MASK) as u32
    }

    fn epoch(self) -> u8 {
        ((self.0 >> (INDEX_BITS + GENERATION_BITS)) & EPOCH_MASK) as u8
    }

    fn has_extra_bits(self) -> bool {
        self.0 >> (INDEX_BITS + GENERATION_BITS + EPOCH_BITS + 4) != 0
    }
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind() {
            Some(kind) if !self.has_extra_bits() => {
                write!(f, "{}#{}.{}", kind.name(), self.index(), self.generation())
            }
            _ => write!(f, "invalid-handle({:#x})", self.0),
        }
    }
}

struct Entry<T> {
    generation: u32,
    value: Option<T>,
}

/// Generational storage for objects of one [`Kind`].
pub struct Pool<T> {
    kind: Kind,
    epoch: u8,
    capacity: u32,
    entries: Vec<Entry<T>>,
    free: Vec<u32>,
    live: u32,
}

impl<T> Pool<T> {
    pub fn new(kind: Kind, epoch: u8, capacity: u32) -> Self {
        Self {
            kind,
            epoch,
            capacity: capacity.min(INDEX_MASK as u32),
            entries: Vec::new(),
            free: Vec::new(),
            live: 0,
        }
    }

    pub fn live(&self) -> u32 {
        self.live
    }

    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Stores `value`. Fails with `LimitExceeded` when the pool is full; the
    /// value is handed back so the caller can release what it owns.
    pub fn insert(&mut self, value: T) -> std::result::Result<Handle, (T, GfxError)> {
        let index = if let Some(index) = self.free.pop() {
            index
        } else if (self.entries.len() as u32) < self.capacity {
            self.entries.push(Entry {
                generation: 1,
                value: None,
            });
            self.entries.len() as u32 - 1
        } else {
            return Err((
                value,
                GfxError::new(
                    ErrorCode::LimitExceeded,
                    format!(
                        "no free {} (limit {}, {} live; destroyed ones are reusable once the GPU has finished with them)",
                        self.kind.name(),
                        self.capacity,
                        self.live
                    ),
                ),
            ));
        };
        let entry = &mut self.entries[index as usize];
        entry.value = Some(value);
        self.live += 1;
        Ok(Handle::new(self.kind, self.epoch, entry.generation, index))
    }

    /// Stores `values` at consecutive indices, so that a shader can address
    /// them as `base + i`. Always takes fresh indices at the end of the pool.
    pub fn insert_run(&mut self, values: Vec<T>) -> Result<Vec<Handle>> {
        let count = values.len() as u32;
        if (self.entries.len() as u32).saturating_add(count) > self.capacity {
            return Err(GfxError::new(
                ErrorCode::LimitExceeded,
                format!(
                    "no room for {count} consecutive {}s (limit {}, {} indices handed out so far)",
                    self.kind.name(),
                    self.capacity,
                    self.entries.len()
                ),
            ));
        }
        let mut handles = Vec::with_capacity(values.len());
        for value in values {
            let index = self.entries.len() as u32;
            self.entries.push(Entry {
                generation: 1,
                value: Some(value),
            });
            self.live += 1;
            handles.push(Handle::new(self.kind, self.epoch, 1, index));
        }
        Ok(handles)
    }

    /// Handles of every live object.
    pub fn live_handles(&self) -> Vec<Handle> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.value.is_some())
            .map(|(index, entry)| {
                Handle::new(self.kind, self.epoch, entry.generation, index as u32)
            })
            .collect()
    }

    /// The live object at a raw index, if any.
    pub fn get_by_index(&self, index: u32) -> Option<&T> {
        self.entries.get(index as usize)?.value.as_ref()
    }

    fn check(&self, handle: Handle) -> Result<usize> {
        let Some(kind) = handle.kind().filter(|_| !handle.has_extra_bits()) else {
            return Err(GfxError::new(
                ErrorCode::InvalidHandle,
                format!(
                    "{handle:?} is not a handle (expected a {})",
                    self.kind.name()
                ),
            ));
        };
        if kind != self.kind {
            return Err(GfxError::new(
                ErrorCode::WrongKind,
                format!("expected a {}, got {handle:?}", self.kind.name()),
            ));
        }
        if handle.epoch() != self.epoch {
            return Err(GfxError::new(
                ErrorCode::InvalidHandle,
                format!("{handle:?} belongs to a previous device"),
            ));
        }
        let index = handle.index() as usize;
        let Some(entry) = self.entries.get(index) else {
            return Err(GfxError::new(
                ErrorCode::InvalidHandle,
                format!("{handle:?} was never created"),
            ));
        };
        if entry.generation != handle.generation() || entry.value.is_none() {
            return Err(GfxError::new(
                ErrorCode::Destroyed,
                format!("{handle:?} has been destroyed"),
            ));
        }
        Ok(index)
    }

    #[inline]
    pub fn get(&self, handle: Handle) -> Result<&T> {
        let index = self.check(handle)?;
        Ok(self.entries[index].value.as_ref().expect("checked"))
    }

    #[inline]
    pub fn get_mut(&mut self, handle: Handle) -> Result<&mut T> {
        let index = self.check(handle)?;
        Ok(self.entries[index].value.as_mut().expect("checked"))
    }

    /// Removes the object. The index is **not** reusable until
    /// [`Pool::release_index`] is called, so the caller can keep it reserved
    /// while the GPU may still refer to it.
    pub fn remove(&mut self, handle: Handle) -> Result<T> {
        let index = self.check(handle)?;
        let entry = &mut self.entries[index];
        entry.generation += 1;
        self.live -= 1;
        Ok(entry.value.take().expect("checked"))
    }

    /// Makes an index returned by a removed handle available again.
    pub fn release_index(&mut self, index: u32) {
        // An index whose generation counter is exhausted is retired for good,
        // so a stale handle can never match a newer object.
        if self.entries[index as usize].generation < MAX_GENERATION {
            self.free.push(index);
        }
    }

    /// Removes every object and returns them. Indices are not released.
    pub fn drain(&mut self) -> Vec<T> {
        self.drain_indexed().into_iter().map(|(_, v)| v).collect()
    }

    /// Like [`Pool::drain`], with the index each object occupied. The caller
    /// releases the indices when they may be reused.
    pub fn drain_indexed(&mut self) -> Vec<(u32, T)> {
        self.live = 0;
        self.entries
            .iter_mut()
            .enumerate()
            .filter_map(|(index, entry)| {
                let value = entry.value.take()?;
                entry.generation += 1;
                Some((index as u32, value))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_and_foreign_handles_are_rejected() {
        let mut buffers: Pool<u32> = Pool::new(Kind::Buffer, 3, 2);
        let mut images: Pool<u32> = Pool::new(Kind::Image, 3, 2);
        let a = buffers.insert(10).unwrap();
        let i = images.insert(20).unwrap();
        assert_eq!(*buffers.get(a).unwrap(), 10);
        assert_eq!(buffers.get(i).unwrap_err().code, ErrorCode::WrongKind);
        assert_eq!(
            buffers.get(Handle::from_bits(0)).unwrap_err().code,
            ErrorCode::InvalidHandle
        );
        assert_eq!(
            buffers.get(Handle::from_bits(-1)).unwrap_err().code,
            ErrorCode::InvalidHandle
        );
        assert_eq!(buffers.remove(a).unwrap(), 10);
        assert_eq!(buffers.get(a).unwrap_err().code, ErrorCode::Destroyed);
        // The index stays reserved until it is released.
        let b = buffers.insert(11).unwrap();
        assert_ne!(b.index(), a.index());
        assert_eq!(
            buffers.insert(12).unwrap_err().1.code,
            ErrorCode::LimitExceeded
        );
        buffers.release_index(a.index());
        let c = buffers.insert(12).unwrap();
        assert_eq!(c.index(), a.index());
        assert_eq!(buffers.get(a).unwrap_err().code, ErrorCode::Destroyed);
        // Handles survive a round trip through an f64.
        assert!(c.to_bits() < (1 << 53));
        assert_eq!(Handle::from_bits(c.to_bits() as f64 as i64), c);
        let other_device: Pool<u32> = Pool::new(Kind::Buffer, 4, 2);
        assert_eq!(
            other_device.get(c).unwrap_err().code,
            ErrorCode::InvalidHandle
        );
    }
}
