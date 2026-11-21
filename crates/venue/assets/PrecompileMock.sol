// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

/// @title PrecompileMock: a read-only stand-in for one HyperEVM L1 read precompile.
///
/// The venue bridge installs this runtime code (`precompile_mock.hex`, compiled
/// from this file with solc 0.8.24, optimizer 200 runs, cancun) at the
/// precompile addresses a forked HyperEVM lacks (0x0800, 0x0801, 0x0803, 0x080f,
/// 0x0810, 0x0813). The answer to a call is stored under the keccak of its
/// calldata: slot `key` holds the answer's byte length, slots `key + 1 + i` its
/// 32-byte words. The bridge writes each answer from the venue sim's own Core
/// state, so the contract holds no logic about any precompile's ABI. A query the
/// bridge has not written (an account the sim never held) answers 128 zero
/// bytes, which every precompile's decoder reads as "nothing".
contract PrecompileMock {
    fallback() external {
        assembly {
            calldatacopy(0, 0, calldatasize())
            let key := keccak256(0, calldatasize())
            let n := sload(key)
            if iszero(n) {
                mstore(0, 0)
                mstore(32, 0)
                mstore(64, 0)
                mstore(96, 0)
                return(0, 128)
            }
            for { let i := 0 } lt(i, n) { i := add(i, 32) } {
                mstore(i, sload(add(add(key, 1), div(i, 32))))
            }
            return(0, n)
        }
    }
}
