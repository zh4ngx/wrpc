# wRPC v0.0.1-draft.1 specification

wRPC is a transport-agnostic protocol designed for asynchronous transmit of WIT function call invocations over network and other means of communication.

wRPC follows client-server model, where peers (servers) may *serve* function and method calls invoked by the other peers (clients).

wRPC relies on [component model value definition encoding] for data encoding on the wire.

## Definitions

### Transport

wRPC makes use of *transports*, which are responsible for establishing a connection between two parties and transferring the wRPC wire protocol data between them.

Examples of supported wRPC transports are: TCP, Unix Domain Sockets, QUIC and NATS.io.

### Indexing

As WIT interfaces and associated values are asynchronous in nature, callers and callees require ability to asynchronously, bidirectionally transfer (portions of) function parameter and result value data.

For example, caller of `wasi:http/outgoing-handler.handle` function MUST be able to simultaneously send data over the passed `output-stream` and well as receive data from the returned `input-stream`. For this purpose wRPC transports MUST be *multiplexed*, i.e. they MUST allow for bidirectional concurrent transfer of multiple data streams.

wRPC uses a concept of "indexing" for differentiating and identifying the data streams used as part of processing of a single WIT function invocation.

An "index" is a sequence of unsigned 32-bit integers and represents a reflective structural path to the value, e.g. a record field or a list element.

Consider the following WIT:

```wit
package wrpc-example:doc@0.1.0;

interface example {
    record rec {
        a: stream<u8>,
        b: u32,
    }

    foo: func(v: rec) -> stream<u8>;
}
```

A path to field `a` in `foo` parameter `v` is defined as a sequence: `[0, 0]`.

The invoker of `foo` MAY choose to send the whole contents of parameter `v`, i.e. `rec` encoded using [component model value definition encoding] on the "root" (synchronous) parameter data channel, in which case `stream<u8>` is sent as `list<u8>`, otherwise, the invoker MAY mark `rec.a` as pending in the encoding of `v` and instead, send it asynchnously over data channel identified by `[0, 0]` (first field in the first parameter).

Similarly, the handler of `foo` MAY either send the complete resulting `stream<u8>` contents as encoded `list<u8>` over the "root" (synchronous) result data channel or asynchronously over data channel identified by `0` (first return value).

The indexing rules are as follows:

- Record fields are indexed in order of their WIT declaration
- Tuple members are indexed in order of their WIT declaration
- Variant members elements are indexed in order of their WIT declaration
- List elements are indexed in the order they appear in the list
- Stream elements are indexed in the order they appear in the stream

### Framing

Some transports (like QUIC or NATS.io) have builtin support for multiplexing, whereas others, like TCP or UDS do not.

wRPC suggests a default framing format for non-multiplexed transports, however individual transport implementations are free to use a custom one.

## Framed stream specification

wRPC framed stream begins with a version byte `0x00` and is followed by a header encoded using [component model value definition encoding]:

```wit
record header {
    instance: string,
    name: string,
}
```

The header MAY be followed by one or more frames encoded using [component model value definition encoding]:

```wit
record frame {
    path: list<u32>,
    data: list<u8>,
}
```

It is assumed that streams using this framing protocol can communicate "closing" to peers using some out-of-band mechanism.

## Transport specifications

### TCP

TCP relies on [Framed stream specification](#framed-stream-specification) to map a single TCP stream to a single wRPC invocation.

The server MUST listen on a TCP socket and client MUST establish a new connection to that socket per each invocation.

The write side of the stream MUST be shutdown as soon as data transfer is done, for example, once the client is done sending encoded parameter buffer and all asynchronous parameters, it MUST shutdown the write side of the stream to signal EOF to the server.

### NATS.io

wRPC protocol operates under assumption that globally-unique IDs can be generated by the caller (client). No particular type of identifier is required by wRPC by specification, but in case of NATS transport, the common NATS inbox concept is assumed to be used throughout this specification.

wRPC NATS subjects assume to be rooted at a particular (optional) prefix, this prefix is configured out-of-band.

#### Invocation lifecycle

On a high level, lifecycle of an arbitrary wRPC invocation looks the following:

1. Server subscribes on a subject `T` corresponding to WIT function or method `F` served by itself
2. Client sends a message on subject `T` carrying, optionally truncated, encoded parameters to function `F` and reply subject `R_c`
3. The server sends a packet with no payload on subject `R_c` with a reply subject `R_s`

Concurrently:

1. Client sends invocation parameter data on `R_s.params` and [indexed](#indexing) subjects derived from it
2. Server begins `F` execution

- If `F` returns, concurrently, server sends invocation return data on `R_c.results` and [indexed](#indexing) subjects derived from it
- If `F` traps or execution is not possible for some other reason, server closes all currently streams by sending a packet with an empty payload

#### Invocation subject scheme

Invocation subjects are defined as:

```
[<prefix>.]?wrpc.0.0.1.<wit-instance>.<wit-function>
```

Where `wit-function` corresponds to the function name as it appears in the WIT. 
For the special case of resource constructors, the resource name is used.

Subject examples:
- `MBGL42DWFPGIEI63P333NCZW5BAGYJGSGLAIB6U7PPXKSXKJK74QTUZM.wrpc.0.0.1.wasi:http/outgoing-handler.handle`
- `NARNEZWUJIOUEDOHI6BDRRFST5W6SHMTQXX5CVOBJC7Z4BQ63S2DKZH6.wrpc.0.0.1.wasi:http/outgoing-handler.handle`
- `VD7C7DD6H5XSIL737EEVTHF7G6EYTMIPQLVOE2BLQDC7TEOGTUZECJYF.wrpc.0.0.1.wasi:http/outgoing-handler.handle`
- `VD7C7DD6H5XSIL737EEVTHF7G6EYTMIPQLVOE2BLQDC7TEOGTUZECJYF.wrpc.0.0.1.wasi:http/outgoing-handler@0.2.1.handle`
- `default.wrpc.0.0.1.wasi:http/outgoing-handler.handle`
- `default.wrpc.0.0.1.wasi:http/types.fields`
- `default.wrpc.0.0.1.wasi:http/types@0.2.0.fields`
- `custom.wrpc.0.0.1.wasi:http/types@0.2.0.fields`

Messages sent on this subject MUST specify the reply inbox subject.

#### Indexing subject scheme

Index path is joined using `.` as the separator

Examples:

- `_INBOX.WMZAFf1AjlpSF3r5e65nFe.dOssD7ON.params.0.1.0`
- `_INBOX.WMZAFf1AjlpSF3r5e65nFe.dOssD7ON.params.0.1.1`
- `_INBOX.WMZAFf1AjlpSF3r5e65nFe.dOssD7ON.params.0.1`
- `_INBOX.WMZAFf1AjlpSF3r5e65nFe.dOssD7ON.params.1.2.1`
- `_INBOX.WMZAFf1AjlpSF3r5e65nFe.dOssD7ON.results.0.0.1`
- `_INBOX.WMZAFf1AjlpSF3r5e65nFe.dOssD7ON.results.0.0`
- `_INBOX.WMZAFf1AjlpSF3r5e65nFe.dOssD7ON.results.0`

## Component model value definition encoding extensions

### Futures

`future<T>` values are encoded as `variant future<T> { pending, ready(T) }`.

In case a future is pending, it's value is transmitted using the parent's index.

For example:
```wit
    foo: func(v: future<bool>);
```

If `v` is pending, encoded `bool` value is sent on index `0` (corresponding to first parameter)

### Streams

`stream<T>` values are encoded as `list<T>` .

In case a stream is pending, it is transmitted as a sequence of `list<T>` chunks using the parent's index.


Each stream MUST finish with an empty `list<T>`.

### Resources

Resources are encoded as opaque byte blobs, `list<u8>` and their meaning is entirely application specific.

[component model value definition encoding]: https://github.com/WebAssembly/component-model/blob/main/design/mvp/Binary.md#-value-definitions
