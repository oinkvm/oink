import copy
import json
import pathlib
import unittest
from schema_compatibility import check

class FrozenSchemaTests(unittest.TestCase):
    def setUp(self):
        self.old=json.loads((pathlib.Path(__file__).parent/"fixtures/v1/schema.json").read_text())
        self.new=copy.deepcopy(self.old)
    def test_changed_limit(self):
        self.new["limits"][0][1]+=1
        with self.assertRaises(ValueError):check(self.old,self.new)
    def test_reserved_message_reuse(self):
        row=copy.deepcopy(self.new["messages"][0]); row["id"]=0x0400
        row["capabilities"]=["oink.future/1"]
        self.new["messages"].append(row)
        with self.assertRaises(ValueError):check(self.old,self.new)
    def test_reserved_stream_reuse(self):
        row=copy.deepcopy(self.new["streams"][0]); row["kind"]=6
        self.new["streams"].append(row)
        with self.assertRaises(ValueError):check(self.old,self.new)
    def test_reserved_capability_unadvertised(self):
        self.new["capabilities"].append("oink.calibration/1")
        with self.assertRaises(ValueError):check(self.old,self.new)
    def test_identical(self):
        check(self.old,self.new)
    def test_label_change(self):
        self.new["labels"][0][1].append(0)
        with self.assertRaises(ValueError):check(self.old,self.new)
    def test_safe_optional_field(self):
        schema=self.new["schemas"][0]
        schema["fields"].append({"key":999,"name":"future","shape":"Option<u32>","absence":"optional"})
        check(self.old,self.new)
    def test_changed_type(self):
        field=self.new["schemas"][0]["fields"][0]
        field["shape"]="u64" if field["shape"]=="u32" else "u32"
        with self.assertRaises(ValueError):check(self.old,self.new)
    def test_required_addition(self):
        self.new["schemas"][0]["fields"].append({"key":999,"name":"future","shape":"u32","absence":"required"})
        with self.assertRaises(ValueError):check(self.old,self.new)
    def test_message_needs_new_capability(self):
        row=copy.deepcopy(self.new["messages"][0]);row["id"]=max(r["id"] for r in self.new["messages"])+1;row["name"]="Future"
        self.new["messages"].append(row)
        with self.assertRaises(ValueError):check(self.old,self.new)
        row["capabilities"].append("oink.future/1");self.new["capabilities"].append("oink.future/1")
        check(self.old,self.new)
    def test_tombstone_reuse(self):
        self.new["messages"][0]["id"]=self.old["tombstones"][0]
        with self.assertRaises(ValueError):check(self.old,self.new)
    def test_enum_renumber(self):
        self.new["enums"][0][1][0][0]=777
        with self.assertRaises(ValueError):check(self.old,self.new)
    def test_baseline_removal(self):
        self.new["baseline"].pop()
        with self.assertRaises(ValueError):check(self.old,self.new)
    def test_retired_register_reuse(self):
        self.new["register_kinds"].append([4,"PauseValue"])
        with self.assertRaises(ValueError):check(self.old,self.new)
    def test_register_renumber(self):
        self.new["register_kinds"][0][0]=8
        with self.assertRaises(ValueError):check(self.old,self.new)
    def test_pause_reason_reserved_value(self):
        next(row[1] for row in self.new["enums"] if row[0]=="PauseReason").append([2,"Future"])
        with self.assertRaises(ValueError):check(self.old,self.new)
    def test_wire_unit_change(self):
        self.new["units"][0][1]="pixels"
        with self.assertRaises(ValueError):check(self.old,self.new)
    def test_reserved_field_cannot_become_optional_appended_field(self):
        schema=next(row for row in self.new["schemas"] if row["name"]=="LocalOptionState")
        schema["fields"].append({"key":4,"name":"future","shape":"Option<u32>","absence":"optional"})
        with self.assertRaises(ValueError):check(self.old,self.new)

if __name__=="__main__":unittest.main()
