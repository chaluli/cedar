# Context

The context element in a Cedar policy provides additional information about the
circumstances of the request being evaluated. This includes details such as the
date and time, IP address, authentication methods, or any custom data relevant
to authorization decisions.

Context attributes are passed at evaluation time and can be referenced in policy conditions.
These attributes are not persisted within Cedar but are provided with each request.
