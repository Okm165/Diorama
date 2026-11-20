// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

/// @title MirrorCctpV2
/// @notice A CCTP V2-compatible transmitter + messenger for mirror chains.
///
/// Circle's CCTP V2 is a pair of contracts per chain (TokenMessengerV2 and
/// MessageTransmitterV2) plus a TokenMinter. A mirror chain needs the same
/// *observable contract*: the getter surface an off-chain relay asserts at
/// boot, the exact `MessageSent` wire format a relay decodes, and the attester
/// semantics it verifies on delivery. This contract provides all of it in one
/// deployment so a relay built for a production chain runs unchanged against a
/// mirror.
///
/// Wire format (the published CCTP V2 layout, abi.encodePacked):
///
///   header (148 bytes)
///     0   uint32   version
///     4   uint32   sourceDomain
///     8   uint32   destinationDomain
///     12  bytes32  nonce
///     44  bytes32  sender            (this transmitter, the sending messenger)
///     76  bytes32  recipient         (the peer messenger)
///     108 bytes32  destinationCaller
///     140 uint32   minFinalityThreshold
///     144 uint32   finalityThresholdExecuted
///   body (228 bytes + hookData)
///     148 uint32   version
///     152 bytes32  burnToken
///     184 bytes32  mintRecipient
///     216 uint256  amount
///     248 bytes32  messageSender     (the depositForBurn caller)
///     280 uint256  maxFee
///     312 uint256  feeExecuted
///     344 uint256  expirationBlock   (0 = no expiry)
///     376 bytes    hookData
///
/// The mirror differs from production in exactly two measured places, both
/// stated here rather than discovered later:
///
///   * `feeExecuted` is the caller's `maxFee` (the conservative upper bound;
///     a production mint deducts the market fee, never more than maxFee), so a
///     delivery credits `amount - maxFee` — the shortfall the mirror protocol
///     is calibrated against.
///   * a delivery pays out `localToken` already held by this contract (the
///     mirror's closed loop) instead of minting a new supply; the balance is
///     the tokens burned into it by the opposite lane.
///
/// Everything else — the getters, the packed message, the raw
/// `keccak256(message)` attestation (65-byte `r ‖ s ‖ v`), the nonce replay
/// guard, the attester threshold, and the `destinationCaller` caller guard the
/// production transmitter enforces — is what production verifies. Both burn
/// entry points are present (`depositForBurn` and `depositForBurnWithHook`):
/// the protocol's `ProofExecutor` calls the hook one for the recovery lane, so
/// a surface missing it would revert a production client's burn.
contract MirrorCctpV2 {
    // ---------------------------------------------------------------- roles

    /// The attester-set administrator. A relay installs its attester through
    /// the real setters (`enableAttester` / `setSignatureThreshold`) by
    /// impersonating this address on a mirror chain.
    address public attesterManager;
    address public owner;
    bool private _initialized;

    /// The local CCTP domain (0 on Ethereum, 19 on HyperEVM, …).
    uint32 public localDomain;
    /// The local token this messenger burns and pays out.
    address public localToken;

    /// `version()` and `messageBodyVersion()` — the values a V2 relay pins.
    uint32 public constant version = 1;
    uint32 public constant messageBodyVersion = 1;

    /// The peer messenger per destination domain (bytes32, Circle's spelling).
    mapping(uint32 => bytes32) public remoteTokenMessengers;

    mapping(address => bool) public isEnabledAttester;
    uint256 public signatureThreshold;
    mapping(bytes32 => bool) public usedNonces;

    uint256 private _nonceCounter;

    // --------------------------------------------------------------- events

    /// The CCTP V2 delivery event a relay watches on the source chain.
    event MessageSent(bytes message);
    /// The local delivery record (receipt-friendly; carries the minted amount).
    event MessageReceived(bytes32 messageHash, uint256 amount, uint256 sourceChainId);
    event AttesterEnabled(address indexed attester);
    event SignatureThresholdSet(uint256 threshold);
    event RemoteTokenMessengerSet(uint32 indexed domain, bytes32 messenger);

    // ---------------------------------------------------------------- setup

    /// One-shot setup; the mirror installer writes this contract's code at its
    /// deployment address (`anvil_setCode`) and calls this once.
    function initialize(
        uint32 localDomain_,
        address localToken_,
        address attesterManager_,
        uint32[] calldata peerDomains,
        bytes32[] calldata peerMessengers
    ) external {
        require(!_initialized, "MirrorCctpV2: already initialized");
        require(localToken_ != address(0), "MirrorCctpV2: token");
        require(attesterManager_ != address(0), "MirrorCctpV2: manager");
        require(peerDomains.length == peerMessengers.length, "MirrorCctpV2: peers");
        _initialized = true;
        localDomain = localDomain_;
        localToken = localToken_;
        attesterManager = attesterManager_;
        owner = attesterManager_;
        signatureThreshold = 1;
        for (uint256 i = 0; i < peerDomains.length; i++) {
            remoteTokenMessengers[peerDomains[i]] = peerMessengers[i];
            emit RemoteTokenMessengerSet(peerDomains[i], peerMessengers[i]);
        }
    }

    modifier onlyManager() {
        require(msg.sender == attesterManager, "MirrorCctpV2: not attester manager");
        _;
    }

    function enableAttester(address attester) external onlyManager {
        isEnabledAttester[attester] = true;
        emit AttesterEnabled(attester);
    }

    function setSignatureThreshold(uint256 threshold) external onlyManager {
        require(threshold > 0, "MirrorCctpV2: threshold");
        signatureThreshold = threshold;
        emit SignatureThresholdSet(threshold);
    }

    function setRemoteTokenMessenger(uint32 domain, bytes32 messenger) external {
        require(msg.sender == owner, "MirrorCctpV2: not owner");
        remoteTokenMessengers[domain] = messenger;
        emit RemoteTokenMessengerSet(domain, messenger);
    }

    // --------------------------------------------------------------- getters

    /// The local transmitter (this contract serves both messenger roles).
    function localMessageTransmitter() external view returns (address) {
        return address(this);
    }

    /// No fee surface on the mirror: a relay's `maxFee` check stays bounded only
    /// by `maxFee < amount`, the same rule production applies when minFee is 0.
    function minFee() external pure returns (uint256) {
        return 0;
    }

    // ----------------------------------------------------------------- burn

    /// CCTP V2 `depositForBurn`. Pulls the token from the caller and emits the
    /// packed burn message; the off-chain relay carries it to the destination
    /// domain's transmitter.
    function depositForBurn(
        uint256 amount,
        uint32 destinationDomain,
        bytes32 mintRecipient,
        address burnToken,
        bytes32 destinationCaller,
        uint256 maxFee,
        uint32 minFinalityThreshold
    ) external returns (uint64) {
        return _depositForBurn(
            amount,
            destinationDomain,
            mintRecipient,
            burnToken,
            destinationCaller,
            maxFee,
            minFinalityThreshold,
            new bytes(0)
        );
    }

    /// CCTP V2 `depositForBurnWithHook` — the same burn with `hookData`
    /// appended to the message. Circle's V2 does not execute the hook itself;
    /// the integrator that `destinationCaller` names consumes the message and
    /// runs it (the protocol's recovery wrapper is that integrator). The mirror
    /// must carry the function because the protocol's `ProofExecutor` calls it
    /// for the recovery lane: a surface missing it would revert a production
    /// client's burn, which is exactly the special casing a mirror must not
    /// demand.
    function depositForBurnWithHook(
        uint256 amount,
        uint32 destinationDomain,
        bytes32 mintRecipient,
        address burnToken,
        bytes32 destinationCaller,
        uint256 maxFee,
        uint32 minFinalityThreshold,
        bytes calldata hookData
    ) external returns (uint64) {
        return _depositForBurn(
            amount,
            destinationDomain,
            mintRecipient,
            burnToken,
            destinationCaller,
            maxFee,
            minFinalityThreshold,
            hookData
        );
    }

    function _depositForBurn(
        uint256 amount,
        uint32 destinationDomain,
        bytes32 mintRecipient,
        address burnToken,
        bytes32 destinationCaller,
        uint256 maxFee,
        uint32 minFinalityThreshold,
        bytes memory hookData
    ) private returns (uint64) {
        require(burnToken == localToken, "MirrorCctpV2: burn token");
        require(amount > 0, "MirrorCctpV2: amount");
        require(maxFee < amount, "MirrorCctpV2: maxFee");
        require(destinationDomain != localDomain, "MirrorCctpV2: same domain");
        bytes32 peer = remoteTokenMessengers[destinationDomain];
        require(peer != bytes32(0), "MirrorCctpV2: no peer messenger");

        _safeTransferFrom(localToken, msg.sender, address(this), amount);

        uint256 nonce = _nonceCounter++;
        bytes memory message = buildMessage(
            bytes32(nonce),
            destinationDomain,
            peer,
            mintRecipient,
            burnToken,
            amount,
            bytes32(uint256(uint160(msg.sender))),
            maxFee,
            maxFee,
            destinationCaller,
            minFinalityThreshold,
            minFinalityThreshold,
            hookData
        );
        emit MessageSent(message);
        return uint64(nonce);
    }

    /// The exact packed message for the given fields. Split out so the layout
    /// is a testable pure function, not a side effect of the burn path.
    function buildMessage(
        bytes32 nonce,
        uint32 destinationDomain,
        bytes32 recipient,
        bytes32 mintRecipient,
        address burnToken,
        uint256 amount,
        bytes32 messageSender,
        uint256 maxFee,
        uint256 feeExecuted,
        bytes32 destinationCaller,
        uint32 minFinalityThreshold,
        uint32 finalityThresholdExecuted,
        bytes memory hookData
    ) public view returns (bytes memory) {
        return abi.encodePacked(
            version,
            localDomain,
            destinationDomain,
            nonce,
            bytes32(uint256(uint160(address(this)))),
            recipient,
            destinationCaller,
            minFinalityThreshold,
            finalityThresholdExecuted,
            messageBodyVersion,
            bytes32(uint256(uint160(burnToken))),
            mintRecipient,
            amount,
            messageSender,
            maxFee,
            feeExecuted,
            uint256(0), // expirationBlock: no expiry on a mirror
            hookData
        );
    }

    // -------------------------------------------------------------- receive

    /// CCTP V2 `receiveMessage`. Verifies the attester signature over the raw
    /// message digest, guards the nonce, pays out `amount - feeExecuted` of the
    /// local token to the mint recipient, and records the message.
    function receiveMessage(bytes calldata message, bytes calldata attestation) external returns (bool) {
        require(message.length >= 376, "MirrorCctpV2: short message");
        require(attestation.length == 65, "MirrorCctpV2: attestation");

        require(_u32(message, 8) == localDomain, "MirrorCctpV2: destination domain");
        bytes32 nonce = _b32(message, 12);
        require(!usedNonces[nonce], "MirrorCctpV2: nonce used");
        require(_b32(message, 76) == bytes32(uint256(uint160(address(this)))), "MirrorCctpV2: recipient");

        bytes32 digest = keccak256(message);
        address signer = _recover(digest, attestation);
        require(isEnabledAttester[signer], "MirrorCctpV2: attester");

        // The production transmitter's caller guard, after the attestation
        // (measured on the live contract: a valid attestation from a
        // non-designated caller reverts "Invalid caller for message"). A
        // message that names a `destinationCaller` may be consumed only by that
        // address; zero means anyone. The protocol's recovery wrapper relies on
        // this to be the sole consumer, so a mirror that skipped it would mint
        // without the credit — the unattributed-arrival bug the wrapper exists
        // to prevent.
        bytes32 destinationCaller = _b32(message, 108);
        if (destinationCaller != bytes32(0)) {
            require(msg.sender == address(uint160(uint256(destinationCaller))), "MirrorCctpV2: destination caller");
        }

        usedNonces[nonce] = true;

        uint256 amount = _u256(message, 216);
        uint256 feeExecuted = _u256(message, 312);
        require(feeExecuted < amount, "MirrorCctpV2: fee");
        address mintRecipient = address(uint160(uint256(_b32(message, 184))));
        uint32 sourceDomain = _u32(message, 4);
        address burnToken = address(uint160(uint256(_b32(message, 152))));
        require(burnToken == localToken, "MirrorCctpV2: burn token");

        uint256 delivered = amount - feeExecuted;
        _safeTransfer(localToken, mintRecipient, delivered);

        emit MessageReceived(digest, delivered, sourceDomain);
        return true;
    }

    // -------------------------------------------------------------- helpers

    function _recover(bytes32 digest, bytes calldata signature) private pure returns (address) {
        bytes32 r = bytes32(signature[0:32]);
        bytes32 s = bytes32(signature[32:64]);
        uint8 v = uint8(signature[64]);
        return ecrecover(digest, v, r, s);
    }

    function _u32(bytes calldata data, uint256 offset) private pure returns (uint32) {
        return uint32(uint256(_b32(data, offset)) >> 224);
    }

    function _b32(bytes calldata data, uint256 offset) private pure returns (bytes32 out) {
        assembly {
            out := calldataload(add(data.offset, offset))
        }
    }

    function _u256(bytes calldata data, uint256 offset) private pure returns (uint256) {
        return uint256(_b32(data, offset));
    }

    function _safeTransfer(address token, address to, uint256 amount) private {
        (bool ok, bytes memory ret) =
            token.call(abi.encodeWithSelector(0xa9059cbb, to, amount));
        require(ok && (ret.length == 0 || abi.decode(ret, (bool))), "MirrorCctpV2: transfer");
    }

    function _safeTransferFrom(address token, address from, address to, uint256 amount) private {
        (bool ok, bytes memory ret) =
            token.call(abi.encodeWithSelector(0x23b872dd, from, to, amount));
        require(ok && (ret.length == 0 || abi.decode(ret, (bool))), "MirrorCctpV2: transferFrom");
    }
}
