"""Versioned plugin service declarations."""
from dataclasses import dataclass
from enum import Enum
import re


@dataclass(frozen=True)
class ServiceKey:
    name: str
    major: int

    def __post_init__(self) -> None:
        if not re.fullmatch(r"[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)*", self.name):
            raise ValueError("invalid service name")
        if type(self.major) is not int or not 1 <= self.major <= 4294967295:
            raise ValueError("service major version must be a positive u32")

    def __str__(self) -> str:
        return f"{self.name}@{self.major}"

    @classmethod
    def parse(cls, value: str) -> "ServiceKey":
        name, separator, major = value.rpartition("@")
        if not separator or not re.fullmatch(r"[1-9][0-9]*", major):
            raise ValueError("service key must be name@major")
        return cls(name, int(major))


class DependencyKind(str, Enum):
    REQUIRED = "required"
    OPTIONAL = "optional"
