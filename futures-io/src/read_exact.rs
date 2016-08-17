use std::io::{self, Read};
use std::mem;

use futures::{Poll, Future};

/// A future which can be used to easily read the entire contents of a stream
/// into a vector.
///
/// Created by the `read_exact` function.
pub struct ReadExact<A, T> {
    state: State<A, T>,
}

enum State<A, T> {
    Reading {
        a: A,
        buf: T,
        pos: usize,
    },
    Empty,
}

/// Creates a future which will read exactly enough bytes to fill `buf`,
/// returning an error if EOF is hit sooner.
///
/// The returned future will resolve to both the I/O stream as well as the
/// buffer once the read operation is completed.
///
/// In the case of an error the buffer and the object will be discarded, with
/// the error yielded. In the case of success the object will be destroyed and
/// the buffer will be returned, with all data read from the stream appended to
/// the buffer.
pub fn read_exact<A, T>(a: A, buf: T) -> ReadExact<A, T>
    where A: Read + 'static,
          T: AsMut<[u8]> + 'static,
{
    ReadExact {
        state: State::Reading {
            a: a,
            buf: buf,
            pos: 0,
        },
    }
}

fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "early eof")
}

impl<A, T> Future for ReadExact<A, T>
    where A: Read + 'static,
          T: AsMut<[u8]> + 'static,
{
    type Item = (A, T);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(A, T), io::Error> {
        match self.state {
            State::Reading { ref mut a, ref mut buf, ref mut pos } => {
                let buf = buf.as_mut();
                while *pos < buf.len() {
                    match a.read(&mut buf[*pos..]) {
                        Ok(0) => return Poll::Err(eof()),
                        Ok(n) => *pos += n,
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            return Poll::NotReady
                        }
                        Err(e) => return Poll::Err(e),
                    }
                }
            }
            State::Empty => panic!("poll a WriteAll after it's done"),
        }

        match mem::replace(&mut self.state, State::Empty) {
            State::Reading { a, buf, .. } => Poll::Ok((a, buf)),
            State::Empty => panic!(),
        }
    }
}
