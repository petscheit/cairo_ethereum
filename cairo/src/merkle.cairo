from starkware.cairo.common.builtin_poseidon.poseidon import poseidon_hash
from starkware.cairo.common.cairo_builtins import PoseidonBuiltin
from starkware.cairo.common.alloc import alloc
from starkware.cairo.common.memcpy import memcpy

// First I tried to combine this with the SHA256 merkle tree, but its hard to generalize.
// Main hurdle is the Uint256 type + the 32 bit chunks sha requires.
// For now best solution is to have a separate implementation for the poseidon merkle tree.
namespace PoseidonMerkleTree {
    func compute_root{
        range_check_ptr, poseidon_ptr: PoseidonBuiltin*, pow2_array: felt*
    }(leafs: felt*, leafs_len: felt) -> felt {
        alloc_locals;

        // ensure we have a power of 2.
        local sqrt: felt;
        %{ 
            import math
            ids.sqrt = int(math.sqrt(ids.leafs_len))
        %}
        // ToDo: this doesnt work obviously. Propose a fix!
        // assert pow2_array[sqrt] = leafs_len;

        let (tree: felt*) = alloc();
        let tree_len = 2 * leafs_len - 1;  // total nodes in the tree

        // copy the leafs to the end of the tree array
        memcpy(dst=tree + (tree_len - leafs_len), src=leafs, len=leafs_len);

        // Calculate number of internal nodes to process
        let internal_nodes = leafs_len - 1;

        // I am experimenting with using two pointers for the same segment here. 
        // This reduces the arithmicals operations per pair hash quie a bit.
        // Will port this to the sha256 merkle tree if it makes sense.

        // Set up initial pointers:
        // tree_ptr starts at the last pair of leaves
        let tree_ptr = tree + tree_len;
        // out_ptr starts where first set of hashes should be written
        let out_ptr = tree + internal_nodes;

        compute_merkle_root_inner_optimized(
            tree_ptr=tree_ptr,
            out_ptr=out_ptr,
            steps=internal_nodes
        );

        // The root will be at the first position of the array
        return [tree];
    }

    func compute_merkle_root_inner_optimized{
        range_check_ptr,
        poseidon_ptr: PoseidonBuiltin*
    }(
        tree_ptr: felt*,   // Points to where we read children for hashing
        out_ptr: felt*,    // Points to where we place the newly computed hash
        steps: felt        // Number of internal nodes to compute
    ) {
        alloc_locals;

        // Base case: no more internal nodes to compute
        if (steps == 0) {
            return ();
        }

        // Move read pointer back by 2 to get the pair to hash
        tempvar new_tree_ptr = tree_ptr - 2;

        // Hash the pair of nodes
        let (node) = poseidon_hash([new_tree_ptr], [new_tree_ptr + 1]);

        // Store result and move write pointer back by 1
        tempvar new_out_ptr = out_ptr - 1;
        assert [new_out_ptr] = node;

        // Continue with remaining nodes
        return compute_merkle_root_inner_optimized(
            tree_ptr=new_tree_ptr,
            out_ptr=new_out_ptr,
            steps=steps - 1
        );
    }
}