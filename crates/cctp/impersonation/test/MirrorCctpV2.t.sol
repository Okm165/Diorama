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

    function _word(bytes memory data, uint256 offset) private pure returns (bytes32 out) {
        require(offset + 32 <= data.length, "read");
        assembly {
            out := mload(add(add(data, 0x20), offset))
        }
    }
}
