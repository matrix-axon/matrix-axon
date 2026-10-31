## Default Permission

Everything the shell's HTTP transport needs: send a request, read or release its body, cancel it, swap in a fresh client after resume, and read the connection counters. Any `http` or `https` URL is reachable, the same reach `http:default` grants today (ADR 0127 § 8).

#### This default permission set includes the following:

- `allow-send`
- `allow-read-body`
- `allow-cancel`
- `allow-drop-body`
- `allow-reset`
- `allow-stats`

## Permission Table

<table>
<tr>
<th>Identifier</th>
<th>Description</th>
</tr>


<tr>
<td>

`axon-http:allow-cancel`

</td>
<td>

Enables the cancel command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`axon-http:deny-cancel`

</td>
<td>

Denies the cancel command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`axon-http:allow-drop-body`

</td>
<td>

Enables the drop_body command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`axon-http:deny-drop-body`

</td>
<td>

Denies the drop_body command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`axon-http:allow-read-body`

</td>
<td>

Enables the read_body command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`axon-http:deny-read-body`

</td>
<td>

Denies the read_body command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`axon-http:allow-reset`

</td>
<td>

Enables the reset command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`axon-http:deny-reset`

</td>
<td>

Denies the reset command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`axon-http:allow-send`

</td>
<td>

Enables the send command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`axon-http:deny-send`

</td>
<td>

Denies the send command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`axon-http:allow-stats`

</td>
<td>

Enables the stats command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`axon-http:deny-stats`

</td>
<td>

Denies the stats command without any pre-configured scope.

</td>
</tr>
</table>
