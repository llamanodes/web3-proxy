use slab::Slab;

#[derive(Default)]
pub struct NodeSlab<T>(Slab<Node<T>>);

#[derive(Default, Debug)]
pub struct LinkedList {
    start: Option<usize>,
    end: Option<usize>,
}

pub struct Node<T> {
    value: T,
    prev: Option<usize>,
    next: Option<usize>,
}

impl LinkedList {
    pub const fn new() -> LinkedList {
        LinkedList {
            start: None,
            end: None,
        }
    }

    #[cfg_attr(feature = "inline-more", inline(always))]
    pub fn push<T>(&mut self, NodeSlab(slab): &mut NodeSlab<T>, value: T) -> usize {
        let index = slab.insert(Node {
            value,
            prev: self.end.or(self.start),
            next: None,
        });

        if let Some(old_end) = self.end.replace(index) {
            slab[old_end].next = Some(index);
        } else {
            self.start = Some(index);
        }

        index
    }

    #[cfg_attr(feature = "inline-more", inline(always))]
    pub fn pop_front<T>(&mut self, NodeSlab(slab): &mut NodeSlab<T>) -> Option<T> {
        let index = self.start?;
        let node = slab.remove(index);

        debug_assert!(node.prev.is_none());

        self.start = node.next;

        if let Some(index) = self.start {
            slab[index].prev.take();
        }

        if Some(index) == self.end {
            self.end.take();
        }

        Some(node.value)
    }

    #[cfg_attr(feature = "inline-more", inline(always))]
    pub fn pop_last<T>(&mut self, NodeSlab(slab): &mut NodeSlab<T>) -> Option<T> {
        let index = self.end?;
        let node = slab.remove(index);

        debug_assert!(node.next.is_none());

        self.end = node.prev;

        if let Some(index) = self.end {
            slab[index].next.take();
        }

        if Some(index) == self.start {
            self.start.take();
        }

        Some(node.value)
    }

    #[cfg_attr(feature = "inline-more", inline(always))]
    pub fn touch<'a, T>(
        &mut self,
        NodeSlab(slab): &'a mut NodeSlab<T>,
        index: usize,
    ) -> Option<&'a mut T> {
        let (node_prev, node_next) = {
            let node = slab.get(index)?;
            (node.prev, node.next)
        };

        if let Some(next) = node_next {
            slab[next].prev = node_prev;
        } else {
            debug_assert_eq!(Some(index), self.end);

            return Some(&mut slab[index].value);
        }

        if let Some(prev) = node_prev {
            slab[prev].next = node_next;
        } else {
            self.start = node_next;
        }

        let end = self.end.replace(index)?;
        slab[end].next = Some(index);

        let node = &mut slab[index];
        node.prev = Some(end);
        node.next.take();

        Some(&mut node.value)
    }

    #[cfg_attr(feature = "inline-more", inline(always))]
    pub fn remove<T>(&mut self, NodeSlab(slab): &mut NodeSlab<T>, index: usize) -> Option<T> {
        let node = if slab.contains(index) {
            // why not return Option :(
            slab.remove(index)
        } else {
            return None;
        };

        if let Some(prev) = node.prev {
            slab[prev].next = node.next;
        } else {
            self.start = node.next;
        }

        if let Some(next) = node.next {
            slab[next].prev = node.prev;
        } else {
            self.end = node.prev;
        }

        Some(node.value)
    }
}

impl<T> NodeSlab<T> {
    #[inline]
    pub fn new() -> NodeSlab<T> {
        NodeSlab(Slab::new())
    }

    #[inline]
    pub fn with_capacity(cap: usize) -> NodeSlab<T> {
        NodeSlab(Slab::with_capacity(cap))
    }

    #[inline]
    pub fn get(&self, index: usize) -> Option<&T> {
        Some(&self.0.get(index)?.value)
    }

    #[inline]
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        Some(&mut self.0.get_mut(index)?.value)
    }

    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.0.reserve(additional);
    }

    #[inline]
    pub fn shrink_to_fit(&mut self) {
        self.0.shrink_to_fit();
    }

    #[inline]
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }
}

#[test]
fn test_linkedlist() {
    let mut slab = NodeSlab::new();
    let mut list = LinkedList::new();

    list.push(&mut slab, 0);
    assert_eq!(Some(0), list.pop_front(&mut slab));
    assert_eq!(None, list.pop_front(&mut slab));

    list.push(&mut slab, 1);
    assert_eq!(Some(1), list.pop_last(&mut slab));
    assert_eq!(None, list.pop_last(&mut slab));

    list.push(&mut slab, 2);
    list.push(&mut slab, 3);
    assert_eq!(Some(2), list.pop_front(&mut slab));
    assert_eq!(Some(3), list.pop_last(&mut slab));
    eprintln!("{:?}", list);
    assert_eq!(None, list.pop_front(&mut slab));
    eprintln!("{:?}", list);
    assert_eq!(None, list.pop_last(&mut slab));

    list.push(&mut slab, 4);
    list.push(&mut slab, 5);
    assert_eq!(Some(5), list.pop_last(&mut slab));
    assert_eq!(Some(4), list.pop_front(&mut slab));
    assert_eq!(None, list.pop_last(&mut slab));
    assert_eq!(None, list.pop_front(&mut slab));

    let index6 = list.push(&mut slab, 6);
    let index7 = list.push(&mut slab, 7);
    let index8 = list.push(&mut slab, 8);
    assert_eq!(Some(7), list.remove(&mut slab, index7));
    assert_eq!(None, list.remove(&mut slab, index7));
    assert_eq!(Some(&6), slab.get(index6));
    assert_eq!(Some(&8), slab.get(index8));
    assert_eq!(Some(6), list.pop_front(&mut slab));
    assert_eq!(Some(8), list.pop_front(&mut slab));

    let index9 = list.push(&mut slab, 9);
    list.push(&mut slab, 10);
    assert_eq!(Some(&mut 9), list.touch(&mut slab, index9));
    assert_eq!(Some(10), list.pop_front(&mut slab));
    assert_eq!(Some(9), list.pop_front(&mut slab));

    let index11 = list.push(&mut slab, 11);
    let index12 = list.push(&mut slab, 12);
    list.push(&mut slab, 13);
    assert_eq!(Some(&mut 12), list.touch(&mut slab, index12));
    assert_eq!(Some(&mut 11), list.touch(&mut slab, index11));
    assert_eq!(Some(13), list.pop_front(&mut slab));
    assert_eq!(Some(12), list.pop_front(&mut slab));
    assert_eq!(Some(11), list.pop_front(&mut slab));

    for i in 0..32 {
        list.push(&mut slab, i);
    }
    for i in 0..32 {
        assert_eq!(Some(i), list.pop_front(&mut slab));
    }
    assert_eq!(None, list.pop_front(&mut slab));

    for i in 0..32 {
        list.push(&mut slab, i);
    }
    for i in (0..32).rev() {
        assert_eq!(Some(i), list.pop_last(&mut slab));
    }
    assert_eq!(None, list.pop_last(&mut slab));
}
