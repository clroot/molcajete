//! Which queued frames still wait for images.
//!
//! A frame of text arrives as dozens of images, and the queue is looked at
//! again after every one of them is decoded. Scanning the frame for pending
//! images each time costs about n²/2 checks for n images, so every frame
//! counts its pending images instead: an [`ImageSlot`] joins the frame it is
//! queued in and leaves it when its decoding ends, either way.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

/// Decoding state of an image referenced by a queued draw.
pub enum ImageState<T> {
    Pending,
    Ready(T),
    Failed,
}

/// Number of images a frame still waits for.
type Waiting = Rc<Cell<usize>>;

struct Shared<T> {
    state: RefCell<ImageState<T>>,
    /// The frame waiting for this image, while it is pending and queued.
    frame: RefCell<Option<Waiting>>,
}

/// Where a queued draw finds its image once it is decoded. Clones share the
/// same image.
pub struct ImageSlot<T>(Rc<Shared<T>>);

impl<T> Clone for ImageSlot<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> ImageSlot<T> {
    /// A slot for an image that is still being decoded.
    pub fn pending() -> Self {
        Self(Rc::new(Shared {
            state: RefCell::new(ImageState::Pending),
            frame: RefCell::new(None),
        }))
    }

    /// Ends decoding with `state` (the image, or failure). The frame the
    /// slot is queued in no longer waits for it.
    pub fn finish(&self, state: ImageState<T>) {
        let done = !matches!(state, ImageState::Pending);
        *self.0.state.borrow_mut() = state;
        if done && let Some(frame) = self.0.frame.borrow_mut().take() {
            frame.set(frame.get() - 1);
        }
    }

    /// Takes the image out, leaving the slot failed.
    pub fn take(&self) -> ImageState<T> {
        let state = std::mem::replace(&mut *self.0.state.borrow_mut(), ImageState::Failed);
        if let Some(frame) = self.0.frame.borrow_mut().take() {
            frame.set(frame.get() - 1);
        }
        state
    }

    fn is_pending(&self) -> bool {
        matches!(*self.0.state.borrow(), ImageState::Pending)
    }
}

/// The frames in the queue, oldest first, each with the number of images it
/// waits for. The last one is the frame still being received.
pub struct Frames {
    waiting: VecDeque<Waiting>,
}

impl Default for Frames {
    fn default() -> Self {
        Self {
            waiting: VecDeque::from([Waiting::default()]),
        }
    }
}

impl Frames {
    /// Adds an image to the frame being received. The frame waits for it
    /// unless its decoding already ended.
    pub fn add_image<T>(&mut self, slot: &ImageSlot<T>) {
        let mut frame = slot.0.frame.borrow_mut();
        if !slot.is_pending() || frame.is_some() {
            return;
        }
        let receiving = self.waiting.back().expect("a frame is being received");
        receiving.set(receiving.get() + 1);
        *frame = Some(receiving.clone());
    }

    /// Ends the frame being received (at its `sync`) and starts the next.
    pub fn end_frame(&mut self) {
        self.waiting.push_back(Waiting::default());
    }

    /// Whether the oldest frame still waits for images.
    pub fn oldest_is_waiting(&self) -> bool {
        self.waiting.front().is_some_and(|w| w.get() > 0)
    }

    /// Forgets the oldest frame, once it has been applied.
    pub fn pop_oldest(&mut self) {
        if self.waiting.len() > 1 {
            self.waiting.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Slot = ImageSlot<u32>;

    /// Queues a frame of `images` pending images and ends it.
    fn frame(frames: &mut Frames, images: usize) -> Vec<Slot> {
        let slots: Vec<Slot> = (0..images).map(|_| Slot::pending()).collect();
        slots.iter().for_each(|slot| frames.add_image(slot));
        frames.end_frame();
        slots
    }

    #[test]
    fn a_frame_waits_until_all_of_its_images_are_decoded() {
        let mut frames = Frames::default();
        let slots = frame(&mut frames, 3);
        // In any order.
        for i in [2, 0] {
            slots[i].finish(ImageState::Ready(i as u32));
            assert!(frames.oldest_is_waiting());
        }
        slots[1].finish(ImageState::Ready(1));
        assert!(!frames.oldest_is_waiting());
        assert!(matches!(slots[2].take(), ImageState::Ready(2)));
        assert!(matches!(slots[2].take(), ImageState::Failed));
    }

    #[test]
    fn a_failed_image_does_not_block_its_frame() {
        let mut frames = Frames::default();
        let slots = frame(&mut frames, 2);
        slots[0].finish(ImageState::Failed);
        assert!(frames.oldest_is_waiting());
        slots[1].finish(ImageState::Ready(1));
        assert!(!frames.oldest_is_waiting());
    }

    #[test]
    fn frames_are_applied_in_order() {
        let mut frames = Frames::default();
        let first = frame(&mut frames, 1);
        let second = frame(&mut frames, 1);
        let _third = frame(&mut frames, 0);
        // The later frame is ready first, but the first one still waits.
        second[0].finish(ImageState::Ready(2));
        assert!(frames.oldest_is_waiting());
        first[0].finish(ImageState::Ready(1));
        assert!(!frames.oldest_is_waiting());
        frames.pop_oldest();
        assert!(!frames.oldest_is_waiting());
        frames.pop_oldest();
        assert!(!frames.oldest_is_waiting());
    }

    #[test]
    fn an_image_counts_for_the_frame_it_was_queued_in() {
        let mut frames = Frames::default();
        let first = frame(&mut frames, 1);
        // Still being received: no `sync` yet.
        let late = Slot::pending();
        frames.add_image(&late);
        first[0].finish(ImageState::Ready(1));
        assert!(!frames.oldest_is_waiting());
        frames.pop_oldest();
        assert!(frames.oldest_is_waiting());
        late.finish(ImageState::Ready(2));
        assert!(!frames.oldest_is_waiting());
    }

    #[test]
    fn an_image_is_counted_once() {
        let mut frames = Frames::default();
        let slot = Slot::pending();
        // The same image drawn twice in one frame.
        frames.add_image(&slot);
        frames.add_image(&slot.clone());
        let done = Slot::pending();
        done.finish(ImageState::Failed);
        frames.add_image(&done);
        frames.end_frame();
        assert!(frames.oldest_is_waiting());
        slot.finish(ImageState::Ready(1));
        slot.finish(ImageState::Failed);
        assert!(!frames.oldest_is_waiting());
    }

    #[test]
    fn images_of_discarded_frames_are_ignored() {
        let mut frames = Frames::default();
        let old = frame(&mut frames, 2);
        // The queue is discarded (disconnect) while images are decoding.
        frames = Frames::default();
        let new = frame(&mut frames, 1);
        old[0].finish(ImageState::Ready(1));
        old[1].finish(ImageState::Failed);
        assert!(frames.oldest_is_waiting());
        new[0].finish(ImageState::Ready(2));
        assert!(!frames.oldest_is_waiting());
    }
}
