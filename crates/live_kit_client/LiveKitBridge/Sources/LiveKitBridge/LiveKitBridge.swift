import Foundation
import LiveKit
import WebRTC
import ScreenCaptureKit

class LKRoomDelegate: RoomDelegate {
    var data: UnsafeRawPointer
    var onDidDisconnect: @convention(c) (UnsafeRawPointer) -> Void
    var onDidSubscribeToRemoteVideoTrack: @convention(c) (UnsafeRawPointer, CFString, CFString, UnsafeRawPointer) -> Void
    var onDidUnsubscribeFromRemoteVideoTrack: @convention(c) (UnsafeRawPointer, CFString, CFString) -> Void
    
    init(
        data: UnsafeRawPointer,
        onDidDisconnect: @escaping @convention(c) (UnsafeRawPointer) -> Void,
        onDidSubscribeToRemoteVideoTrack: @escaping @convention(c) (UnsafeRawPointer, CFString, CFString, UnsafeRawPointer) -> Void,
        onDidUnsubscribeFromRemoteVideoTrack: @escaping @convention(c) (UnsafeRawPointer, CFString, CFString) -> Void)
    {
        self.data = data
        self.onDidDisconnect = onDidDisconnect
        self.onDidSubscribeToRemoteVideoTrack = onDidSubscribeToRemoteVideoTrack
        self.onDidUnsubscribeFromRemoteVideoTrack = onDidUnsubscribeFromRemoteVideoTrack
    }

    func room(_ room: Room, didUpdate connectionState: ConnectionState, oldValue: ConnectionState) {
        if connectionState.isDisconnected {
            self.onDidDisconnect(self.data)
        }
    }

    func room(_ room: Room, participant: RemoteParticipant, didSubscribe publication: RemoteTrackPublication, track: Track) {
        if track.kind == .video {
            self.onDidSubscribeToRemoteVideoTrack(self.data, participant.identity as CFString, track.sid! as CFString, Unmanaged.passUnretained(track).toOpaque())
        }
    }
    
    func room(_ room: Room, participant: RemoteParticipant, didUnsubscribe publication: RemoteTrackPublication, track: Track) {
        if track.kind == .video {
            self.onDidUnsubscribeFromRemoteVideoTrack(self.data, participant.identity as CFString, track.sid! as CFString)
        }
    }
}

class LKVideoRenderer: NSObject, VideoRenderer {
    var data: UnsafeRawPointer
    var onFrame: @convention(c) (UnsafeRawPointer, CVPixelBuffer) -> Bool
    var onDrop: @convention(c) (UnsafeRawPointer) -> Void
    var adaptiveStreamIsEnabled: Bool = false
    var adaptiveStreamSize: CGSize = .zero
    weak var track: VideoTrack?

    init(data: UnsafeRawPointer, onFrame: @escaping @convention(c) (UnsafeRawPointer, CVPixelBuffer) -> Bool, onDrop: @escaping @convention(c) (UnsafeRawPointer) -> Void) {
        self.data = data
        self.onFrame = onFrame
        self.onDrop = onDrop
    }

    deinit {
        self.onDrop(self.data)
    }

    func setSize(_ size: CGSize) {
    }

    func renderFrame(_ frame: RTCVideoFrame?) {
        let buffer = frame?.buffer as? RTCCVPixelBuffer
        if let pixelBuffer = buffer?.pixelBuffer {
            if !self.onFrame(self.data, pixelBuffer) {
                DispatchQueue.main.async {
                    self.track?.remove(videoRenderer: self)
                }
            }
        }
    }
}

@_cdecl("LKRoomDelegateCreate")
public func LKRoomDelegateCreate(
    data: UnsafeRawPointer,
    onDidDisconnect: @escaping @convention(c) (UnsafeRawPointer) -> Void,
    onDidSubscribeToRemoteVideoTrack: @escaping @convention(c) (UnsafeRawPointer, CFString, CFString, UnsafeRawPointer) -> Void,
    onDidUnsubscribeFromRemoteVideoTrack: @escaping @convention(c) (UnsafeRawPointer, CFString, CFString) -> Void
) -> UnsafeMutableRawPointer {
    let delegate = LKRoomDelegate(
        data: data,
        onDidDisconnect: onDidDisconnect,
        onDidSubscribeToRemoteVideoTrack: onDidSubscribeToRemoteVideoTrack,
        onDidUnsubscribeFromRemoteVideoTrack: onDidUnsubscribeFromRemoteVideoTrack
    )
    return Unmanaged.passRetained(delegate).toOpaque()
}

@_cdecl("LKRoomCreate")
public func LKRoomCreate(delegate: UnsafeRawPointer) -> UnsafeMutableRawPointer  {
    let delegate = Unmanaged<LKRoomDelegate>.fromOpaque(delegate).takeUnretainedValue()
    return Unmanaged.passRetained(Room(delegate: delegate)).toOpaque()
}

@_cdecl("LKRoomConnect")
public func LKRoomConnect(room: UnsafeRawPointer, url: CFString, token: CFString, callback: @escaping @convention(c) (UnsafeRawPointer, CFString?) -> Void, callback_data: UnsafeRawPointer) {
    let room = Unmanaged<Room>.fromOpaque(room).takeUnretainedValue()

    room.connect(url as String, token as String).then { _ in
        callback(callback_data, UnsafeRawPointer(nil) as! CFString?)
    }.catch { error in
        callback(callback_data, error.localizedDescription as CFString)
    }
}

@_cdecl("LKRoomDisconnect")
public func LKRoomDisconnect(room: UnsafeRawPointer) {
    let room = Unmanaged<Room>.fromOpaque(room).takeUnretainedValue()
    room.disconnect()
}

@_cdecl("LKRoomPublishVideoTrack")
public func LKRoomPublishVideoTrack(room: UnsafeRawPointer, track: UnsafeRawPointer, callback: @escaping @convention(c) (UnsafeRawPointer, UnsafeMutableRawPointer?, CFString?) -> Void, callback_data: UnsafeRawPointer) {
    let room = Unmanaged<Room>.fromOpaque(room).takeUnretainedValue()
    let track = Unmanaged<LocalVideoTrack>.fromOpaque(track).takeUnretainedValue()
    room.localParticipant?.publishVideoTrack(track: track).then { publication in
        callback(callback_data, Unmanaged.passRetained(publication).toOpaque(), nil)
    }.catch { error in
        callback(callback_data, nil, error.localizedDescription as CFString)
    }
}

@_cdecl("LKRoomUnpublishTrack")
public func LKRoomUnpublishTrack(room: UnsafeRawPointer, publication: UnsafeRawPointer) {
    let room = Unmanaged<Room>.fromOpaque(room).takeUnretainedValue()
    let publication = Unmanaged<LocalTrackPublication>.fromOpaque(publication).takeUnretainedValue()
    let _ = room.localParticipant?.unpublish(publication: publication)
}

@_cdecl("LKRoomVideoTracksForRemoteParticipant")
public func LKRoomVideoTracksForRemoteParticipant(room: UnsafeRawPointer, participantId: CFString) -> CFArray? {
    let room = Unmanaged<Room>.fromOpaque(room).takeUnretainedValue()
    
    for (_, participant) in room.remoteParticipants {
        if participant.identity == participantId as String {
            return participant.videoTracks.compactMap { $0.track as? RemoteVideoTrack } as CFArray?
        }
    }
    
    return nil;
}

@_cdecl("LKCreateScreenShareTrackForDisplay")
public func LKCreateScreenShareTrackForDisplay(display: UnsafeMutableRawPointer) -> UnsafeMutableRawPointer {
    let display = Unmanaged<MacOSDisplay>.fromOpaque(display).takeUnretainedValue()
    let track = LocalVideoTrack.createMacOSScreenShareTrack(source: display, preferredMethod: .legacy)
    return Unmanaged.passRetained(track).toOpaque()
}

@_cdecl("LKVideoRendererCreate")
public func LKVideoRendererCreate(data: UnsafeRawPointer, onFrame: @escaping @convention(c) (UnsafeRawPointer, CVPixelBuffer) -> Bool, onDrop: @escaping @convention(c) (UnsafeRawPointer) -> Void) -> UnsafeMutableRawPointer {
    Unmanaged.passRetained(LKVideoRenderer(data: data, onFrame: onFrame, onDrop: onDrop)).toOpaque()
}

@_cdecl("LKVideoTrackAddRenderer")
public func LKVideoTrackAddRenderer(track: UnsafeRawPointer, renderer: UnsafeRawPointer) {
    let track = Unmanaged<Track>.fromOpaque(track).takeUnretainedValue() as! VideoTrack
    let renderer = Unmanaged<LKVideoRenderer>.fromOpaque(renderer).takeRetainedValue()
    renderer.track = track
    track.add(videoRenderer: renderer)
}

@_cdecl("LKRemoteVideoTrackGetSid")
public func LKRemoteVideoTrackGetSid(track: UnsafeRawPointer) -> CFString {
    let track = Unmanaged<RemoteVideoTrack>.fromOpaque(track).takeUnretainedValue()
    return track.sid! as CFString
}

@_cdecl("LKDisplaySources")
public func LKDisplaySources(data: UnsafeRawPointer, callback: @escaping @convention(c) (UnsafeRawPointer, CFArray?, CFString?) -> Void) {
    MacOSScreenCapturer.sources(for: .display, includeCurrentApplication: false, preferredMethod: .legacy).then { displaySources in
        callback(data, displaySources as CFArray, nil)
    }.catch { error in
        callback(data, nil, error.localizedDescription as CFString)
    }
}
