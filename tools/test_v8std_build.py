"""Local, network-free checks for v8std paragraph boundaries."""

import runpy
import unittest
from pathlib import Path

from bs4 import BeautifulSoup


BUILD = runpy.run_path(str(Path(__file__).with_name("v8std-build.py")))


class V8StdBuildTests(unittest.TestCase):
    def test_numbered_items_after_double_break_get_distinct_addresses(self) -> None:
        soup = BeautifulSoup(
            "<body><p>5.2. Transaction rule<br/><br/>6. Line length rule</p></body>",
            "html.parser",
        )
        blocks = BUILD["chunks"](soup, "456", "Module text")
        self.assertEqual([(b[0], b[2]) for b in blocks],
                         [("456:1", "5.2."), ("456:2", "6.")])
        self.assertEqual(" ".join(b[4] for b in blocks),
                         "5.2. Transaction rule 6. Line length rule")

    def test_ordinary_double_break_keeps_one_item(self) -> None:
        soup = BeautifulSoup(
            "<body><p>3. Rule<br/><br/>Long explanation without a new number.</p></body>",
            "html.parser",
        )
        blocks = BUILD["chunks"](soup, "456", "Module text")
        self.assertEqual(len(blocks), 1)
        self.assertEqual(blocks[0][2], "3.")

    def test_table_and_code_example_are_kept_in_source_order(self) -> None:
        soup = BeautifulSoup(
            "<body><p>1. First</p><table><tr><td><p>2. Second</p></td>"
            "<td>Table note</td></tr></table><pre>Example code</pre>"
            "<script>Ignore me</script><p>3. Third</p></body>",
            "html.parser",
        )
        blocks = BUILD["chunks"](soup, "456", "Module text")
        self.assertEqual([b[2] for b in blocks], ["1.", "2.", "3."])
        self.assertIn("Table note", blocks[1][4])
        self.assertIn("Example code", blocks[1][4])
        self.assertNotIn("Ignore me", " ".join(b[4] for b in blocks))

    def test_html_comment_is_not_article_text(self) -> None:
        soup = BeautifulSoup(
            "<body><p>1. Rule</p><!-- LI><A href='next'>Hidden link</A> -->"
            "<p>2. Next rule</p></body>", "html.parser",
        )
        blocks = BUILD["chunks"](soup, "551", "Standards")
        self.assertEqual([b[2] for b in blocks], ["1.", "2."])
        self.assertNotIn("Hidden link", " ".join(b[4] for b in blocks))


if __name__ == "__main__":
    unittest.main()
