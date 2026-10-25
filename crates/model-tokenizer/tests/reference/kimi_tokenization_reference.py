# Extracted (AST only; behavior unchanged) from pinned Kimi public reference.
# Source: https://huggingface.co/moonshotai/Kimi-K3/blob/a590ce090cb049c93a33dfe8c208ec652aa20503/tokenization_kimi.py
# Upstream source SHA256: f28ea66e2d862a2a5814970b2ce40c2f7d8296ff09aed90a7e7def689b906944
# Upstream license: see assets/kimi-k3-tokenizer-config.provenance.json
from typing import List, Iterator

class ReferenceTokenizer:
    pat_str = '|'.join(['[\\p{Han}]+', "[^\\r\\n\\p{L}\\p{N}]?[\\p{Lu}\\p{Lt}\\p{Lm}\\p{Lo}\\p{M}&&[^\\p{Han}]]*[\\p{Ll}\\p{Lm}\\p{Lo}\\p{M}&&[^\\p{Han}]]+(?i:'s|'t|'re|'ve|'m|'ll|'d)?", "[^\\r\\n\\p{L}\\p{N}]?[\\p{Lu}\\p{Lt}\\p{Lm}\\p{Lo}\\p{M}&&[^\\p{Han}]]+[\\p{Ll}\\p{Lm}\\p{Lo}\\p{M}&&[^\\p{Han}]]*(?i:'s|'t|'re|'ve|'m|'ll|'d)?", '\\p{N}{1,3}', ' ?[^\\s\\p{L}\\p{N}]+[\\r\\n]*', '\\s*[\\r\\n]+', '\\s+(?!\\S)', '\\s+'])

    def _encode_text_piece(self, text: str, allow_special_tokens: bool=True) -> List[int]:
        TIKTOKEN_MAX_ENCODE_CHARS = 400000
        MAX_NO_WHITESPACES_CHARS = 25000
        t: List[int] = []
        for i in range(0, len(text), TIKTOKEN_MAX_ENCODE_CHARS):
            for substr in self._split_whitespaces_or_nonwhitespaces(text[i:i + TIKTOKEN_MAX_ENCODE_CHARS], MAX_NO_WHITESPACES_CHARS):
                if allow_special_tokens:
                    t.extend(self.model.encode(substr, allowed_special='all'))
                else:
                    t.extend(self.model.encode(substr, disallowed_special=()))
        return t

    @staticmethod
    def _split_whitespaces_or_nonwhitespaces(s: str, max_consecutive_slice_len: int) -> Iterator[str]:
        """
        Splits the string `s` so that each substring contains no more than `max_consecutive_slice_len`
        consecutive whitespaces or consecutive non-whitespaces.
        """
        current_slice_len = 0
        current_slice_is_space = s[0].isspace() if len(s) > 0 else False
        slice_start = 0
        for i in range(len(s)):
            is_now_space = s[i].isspace()
            if current_slice_is_space ^ is_now_space:
                current_slice_len = 1
                current_slice_is_space = is_now_space
            else:
                current_slice_len += 1
                if current_slice_len > max_consecutive_slice_len:
                    yield s[slice_start:i]
                    slice_start = i
                    current_slice_len = 1
        yield s[slice_start:]
