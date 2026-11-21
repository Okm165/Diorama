// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {MirrorCctpV2} from "../src/MirrorCctpV2.sol";

/// The wire-format pins: every offset below is what the off-chain relay's
/// decoder reads (its own header/body constants). A drift here is a message
/// the relay rejects as malformed — the failure the mirror already paid for
/// once. Plain `require`s: no forge-std, no network, no cheatcodes.
contract MirrorCctpV2LayoutTest {
    MirrorCctpV2 private cctp;

    function setUp() public {
        cctp = new MirrorCctpV2();
        uint32[] memory domains = new uint32[](1);
        bytes32[] memory messengers = new bytes32[](1);
        domains[0] = 19;
        messengers[0] = bytes32(uint256(uint160(0x28b5a0e9C621a5BadaA536219b3a228C8168cf5d)));
        cctp.initialize(0, address(0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48), address(0xBEEF), domains, messengers);
    }

    function test_versionSurface() public view {
        require(cctp.version() == 1, "version");
        require(cctp.messageBodyVersion() == 1, "body version");
        require(cctp.localDomain() == 0, "domain");
        require(cctp.localMessageTransmitter() == address(cctp), "transmitter");
        require(cctp.minFee() == 0, "minFee");
        require(cctp.signatureThreshold() == 1, "threshold");
        require(
            cctp.remoteTokenMessengers(19)
                == bytes32(uint256(uint160(0x28b5a0e9C621a5BadaA536219b3a228C8168cf5d))),
            "peer"
        );
    }

    function test_messageLayoutIsV2() public view {
        bytes memory message = cctp.buildMessage(
            bytes32(uint256(7)),
            19,
            bytes32(uint256(uint160(0x28b5a0e9C621a5BadaA536219b3a228C8168cf5d))),
            bytes32(uint256(uint160(0xf7230f56DFa8F9851B83cA67776dBaC1e4a5C3Be))),
            0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48,
            434644924,
            bytes32(uint256(uint160(0x90CE8b7982499ac8e0FfE2EC20D656Bd78F976b3))),
            71173,
            71173,
            bytes32(0),
            1000,
            1000,
            new bytes(0)
        );
        require(message.length == 376, "length");
        require(uint32(uint256(_word(message, 0)) >> 224) == 1, "header.version");
        require(uint32(uint256(_word(message, 4)) >> 224) == 0, "header.sourceDomain");
        require(uint32(uint256(_word(message, 8)) >> 224) == 19, "header.destinationDomain");
        require(_word(message, 12) == bytes32(uint256(7)), "header.nonce");
        require(
            address(uint160(uint256(_word(message, 44)))) == address(cctp),
            "header.sender"
        );
        require(
            address(uint160(uint256(_word(message, 76)))) == 0x28b5a0e9C621a5BadaA536219b3a228C8168cf5d,
            "header.recipient"
        );
        require(uint32(uint256(_word(message, 140)) >> 224) == 1000, "header.minFinality");
        require(uint32(uint256(_word(message, 144)) >> 224) == 1000, "header.finalityExecuted");
        require(uint32(uint256(_word(message, 148)) >> 224) == 1, "body.version");
        require(
            address(uint160(uint256(_word(message, 152)))) == 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48,
            "body.burnToken"
        );
        require(
            address(uint160(uint256(_word(message, 184)))) == 0xf7230f56DFa8F9851B83cA67776dBaC1e4a5C3Be,
            "body.mintRecipient"
        );
        require(uint256(_word(message, 216)) == 434644924, "body.amount");
        require(
            address(uint160(uint256(_word(message, 248)))) == 0x90CE8b7982499ac8e0FfE2EC20D656Bd78F976b3,
            "body.messageSender"
        );
        require(uint256(_word(message, 280)) == 71173, "body.maxFee");
        require(uint256(_word(message, 312)) == 71173, "body.feeExecuted");
        require(uint256(_word(message, 344)) == 0, "body.expiration");
    }

    /// The hook lane's layout: a hook burn appends `hookData` verbatim after the
    /// 228-byte body (offset 376), and the header's `destinationCaller` word is
    /// where the delivery guard reads it (offset 108). The protocol's
    /// `ProofExecutor.depositForBurnWithHook` + `RecoveryHookWrapper` depend on
    /// both offsets; the live production contract serves them, so the fallback
    /// must too.
    function test_hookBurnAppendsHookDataAt376() public view {
        bytes memory hook = hex"deadbeefcafe";
        bytes32 caller = bytes32(uint256(uint160(0x000000000000000000000000000000000000dEaD)));
        bytes memory message = cctp.buildMessage(
            bytes32(uint256(8)),
            19,
            bytes32(uint256(uint160(0x28b5a0e9C621a5BadaA536219b3a228C8168cf5d))),
            bytes32(uint256(uint160(0xf7230f56DFa8F9851B83cA67776dBaC1e4a5C3Be))),
            0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48,
            1_000_000,
            bytes32(uint256(uint160(0x90CE8b7982499ac8e0FfE2EC20D656Bd78F976b3))),
            0,
            0,
            caller,
            1000,
            1000,
            hook
        );
        require(message.length == 376 + hook.length, "hook length");
        require(_word(message, 108) == caller, "header.destinationCaller");
        for (uint256 i = 0; i < hook.length; i++) {
            require(message[376 + i] == hook[i], "hook data");
        }
    }

    function _word(bytes memory data, uint256 offset) private pure returns (bytes32 out) {
        require(offset + 32 <= data.length, "read");
        assembly {
            out := mload(add(add(data, 0x20), offset))
        }
    }
}

/// The two delivery-path facts a production client relies on, exercised
/// end to end: a burn through **both** entry points moves the token, and a
/// message naming a `destinationCaller` is consumable only by that caller.
///
/// This contract uses the two cheatcodes the flow needs (`sign`, `prank`) and
/// nothing else — no forge-std. The token is a five-function stand-in for the
/// six `IERC20` calls the contract makes.
interface Vm {
    function prank(address sender) external;
    function sign(uint256 privateKey, bytes32 digest) external returns (uint8 v, bytes32 r, bytes32 s);
    function addr(uint256 privateKey) external returns (address);
}

contract TestToken {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        allowance[from][msg.sender] -= amount;
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        return true;
    }
}

contract MirrorCctpV2DeliveryTest {
    Vm private constant VM = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));
    uint256 private constant ATTESTER_KEY = 0xA11CE;

    MirrorCctpV2 private cctp;
    TestToken private token;

    function setUp() public {
        cctp = new MirrorCctpV2();
        token = new TestToken();
        uint32[] memory domains = new uint32[](1);
        bytes32[] memory messengers = new bytes32[](1);
        domains[0] = 19;
        messengers[0] = bytes32(uint256(uint160(address(this))));
        cctp.initialize(0, address(token), address(this), domains, messengers);
        cctp.enableAttester(VM.addr(ATTESTER_KEY));
        token.mint(address(this), 1_000_000);
        token.approve(address(cctp), type(uint256).max);
    }

    /// Both burn entry points exist and move the token: the plain one and the
    /// hook one the protocol's `ProofExecutor` calls for the recovery lane.
    function test_both_burnEntryPointsPullTheToken() public {
        uint64 first = cctp.depositForBurn(1_000, 19, _recipient(), address(token), bytes32(0), 0, 2000);
        uint64 second =
            cctp.depositForBurnWithHook(2_000, 19, _recipient(), address(token), bytes32(0), 0, 2000, hex"deadbeef");
        require(second == first + 1, "hook burn nonce");
        require(token.balanceOf(address(cctp)) == 3_000, "burns pulled");
        require(token.balanceOf(address(this)) == 997_000, "caller debited");
    }

    /// A message naming a `destinationCaller` may be consumed only by it, with
    /// a valid attestation — the production order (attestation first, then the
    /// caller guard), measured on the live contract.
    function test_destinationCallerIsTheOnlyConsumer() public {
        address designated = VM.addr(0xB0B);
        bytes32 nonce = bytes32(uint256(9));
        bytes memory message = cctp.buildMessage(
            nonce,
            0, // destination domain: this chain
            bytes32(uint256(uint160(address(cctp)))), // recipient: the local transmitter
            _recipient(),
            address(token),
            1_000,
            bytes32(uint256(uint160(address(this)))),
            0,
            0,
            bytes32(uint256(uint160(designated))),
            2000,
            2000,
            new bytes(0)
        );
        (uint8 v, bytes32 r, bytes32 s) = VM.sign(ATTESTER_KEY, keccak256(message));
        bytes memory attestation = abi.encodePacked(r, s, v);
        // The mirror's payout is the balance the opposite lane burned into the
        // contract; this test seeds it directly.
        token.mint(address(cctp), 1_000);

        address stranger = VM.addr(0xBAD);
        VM.prank(stranger);
        (bool refused,) = address(cctp).call(abi.encodeWithSignature("receiveMessage(bytes,bytes)", message, attestation));
        require(!refused, "a non-designated caller must be refused");

        VM.prank(designated);
        require(cctp.receiveMessage(message, attestation), "the designated caller delivers");
        require(cctp.usedNonces(nonce), "nonce consumed");
    }

    function _recipient() private returns (bytes32) {
        return bytes32(uint256(uint160(VM.addr(0xFEE))));
    }
}
